use std::fmt::Write as _;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::{Profiler, ScopeReport, TimeSeries};

/// The longest request line accepted before the client is disconnected.
const MAX_REQUEST: usize = 1024;

/// Answers profile requests over TCP, so a tool such as an MCP server can read a live profile.
/// Each request is a line, `snapshot`, `recent` or `series`, and each reply one line of JSON.
/// The profiler is single-threaded, so the app calls [`ProfileServer::poll`] on its own
/// thread, once a frame for example; requests wait until the next poll.
pub struct ProfileServer {
    listener: TcpListener,
    clients: Vec<Client>,
}

struct Client {
    stream: TcpStream,
    /// Bytes received since the last complete request line.
    unread: Vec<u8>,
}

impl ProfileServer {
    pub fn bind(address: impl ToSocketAddrs) -> io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            clients: Vec::new(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept new connections and answer every complete request, without blocking.
    pub fn poll(&mut self, profiler: &Profiler) {
        while let Ok((stream, _)) = self.listener.accept() {
            if let Ok(client) = Client::new(stream) {
                self.clients.push(client);
            }
        }
        self.clients
            .retain_mut(|client| client.serve(profiler).is_ok());
    }
}

impl Client {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        Ok(Self {
            stream,
            unread: Vec::new(),
        })
    }

    /// Answer the requests received so far. An error means the client is done or misbehaving.
    fn serve(&mut self, profiler: &Profiler) -> io::Result<()> {
        let finished_writing = self.receive()?;
        while let Some(end) = self.unread.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.unread.drain(..=end).collect();
            let reply = respond(profiler, String::from_utf8_lossy(&line).trim());
            // A reply can outgrow the socket buffer, so write it whole before reading on.
            self.stream.set_nonblocking(false)?;
            self.stream.write_all(reply.as_bytes())?;
            self.stream.set_nonblocking(true)?;
        }
        if finished_writing {
            Err(ErrorKind::UnexpectedEof.into())
        } else if self.unread.len() > MAX_REQUEST {
            Err(ErrorKind::InvalidData.into())
        } else {
            Ok(())
        }
    }

    /// Read what has arrived, returning whether the client has stopped writing.
    fn receive(&mut self) -> io::Result<bool> {
        let mut chunk = [0; MAX_REQUEST];
        while self.unread.len() <= MAX_REQUEST {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(true),
                Ok(read) => self.unread.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}

fn respond(profiler: &Profiler, request: &str) -> String {
    let mut json = String::new();
    match request {
        "snapshot" => {
            json.push_str("{\"scopes\":");
            write_scopes(&mut json, &profiler.snapshot());
        }
        "recent" => {
            let window = milliseconds(profiler.window());
            write!(json, "{{\"window_ms\":{window},\"scopes\":").unwrap();
            write_scopes(&mut json, &profiler.recent_snapshot());
        }
        "series" => write_series(&mut json, &profiler.recent_series()),
        _ => {
            json.push_str("{\"error\":");
            let message = format!("unknown request \"{request}\"; try snapshot, recent or series");
            write_string(&mut json, &message);
        }
    }
    json.push_str("}\n");
    json
}

fn write_scopes(json: &mut String, scopes: &[ScopeReport]) {
    json.push('[');
    for (position, scope) in scopes.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        json.push_str("{\"name\":");
        write_string(json, scope.name);
        let total = milliseconds(scope.total);
        let self_time = milliseconds(scope.self_time());
        write!(
            json,
            ",\"hits\":{},\"total_ms\":{total},\"self_ms\":{self_time},\"children\":",
            scope.hits
        )
        .unwrap();
        write_scopes(json, &scope.children);
        json.push('}');
    }
    json.push(']');
}

fn write_series(json: &mut String, series: &TimeSeries) {
    let bucket = milliseconds(series.bucket);
    write!(json, "{{\"bucket_ms\":{bucket},\"scopes\":[").unwrap();
    for (position, scope) in series.scopes.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        json.push_str("{\"path\":[");
        for (position, name) in scope.path.iter().enumerate() {
            if position > 0 {
                json.push(',');
            }
            write_string(json, name);
        }
        json.push_str("],\"self_ms\":[");
        let self_times: Vec<String> = scope
            .self_times
            .iter()
            .map(|&time| milliseconds(time))
            .collect();
        json.push_str(&self_times.join(","));
        json.push_str("]}");
    }
    json.push(']');
}

fn milliseconds(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1000.0)
}

fn write_string(json: &mut String, text: &str) {
    json.push('"');
    for character in text.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            character if character.is_control() => {
                write!(json, "\\u{:04x}", character as u32).unwrap();
            }
            character => json.push(character),
        }
    }
    json.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::millisecond_profiler;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpStream};
    use std::time::{Duration, Instant};

    fn connect(server: &ProfileServer) -> TcpStream {
        let client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        client
    }

    /// Poll the server until a line, or the end of the connection, arrives.
    fn read_reply(
        server: &mut ProfileServer,
        profiler: &Profiler,
        reader: &mut BufReader<TcpStream>,
    ) -> String {
        let mut reply = String::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            server.poll(profiler);
            if reader.read_line(&mut reply).is_ok() {
                return reply;
            }
        }
        panic!("no reply after {reply:?}");
    }

    /// Send `request` and poll the server until a reply line arrives.
    fn ask(server: &mut ProfileServer, profiler: &Profiler, request: &str) -> String {
        let mut client = connect(server);
        writeln!(client, "{request}").unwrap();
        read_reply(server, profiler, &mut BufReader::new(client))
    }

    #[test]
    fn a_request_is_answered_after_the_client_stops_writing() {
        let profiler = millisecond_profiler();
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();
        let mut client = connect(&server);
        writeln!(client, "snapshot").unwrap();
        client.shutdown(Shutdown::Write).unwrap();

        let reply = read_reply(&mut server, &profiler, &mut BufReader::new(client));

        assert_eq!(reply, "{\"scopes\":[]}\n");
    }

    #[test]
    fn requests_split_across_polls_are_answered_in_order() {
        let profiler = millisecond_profiler();
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();
        let mut client = connect(&server);
        write!(client, "snap").unwrap();
        server.poll(&profiler);
        write!(client, "shot\nnope\n").unwrap();
        let mut reader = BufReader::new(client);

        let first = read_reply(&mut server, &profiler, &mut reader);
        let second = read_reply(&mut server, &profiler, &mut reader);

        assert_eq!(first, "{\"scopes\":[]}\n");
        assert!(second.starts_with("{\"error\":"), "{second}");
    }

    #[test]
    fn a_client_sending_an_overlong_line_is_disconnected() {
        let profiler = millisecond_profiler();
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();
        let mut client = connect(&server);
        client.write_all(&[b'x'; 2 * MAX_REQUEST]).unwrap();

        let reply = read_reply(&mut server, &profiler, &mut BufReader::new(client));

        assert_eq!(reply, "", "the connection should end without a reply");
    }

    #[test]
    fn snapshot_replies_with_the_scope_tree_as_json() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["frame", "sort"], 1_000, 3, 3);
        profiler.record(vec!["frame"], 1_000, 5, 2);
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();

        assert_eq!(
            ask(&mut server, &profiler, "snapshot"),
            concat!(
                r#"{"scopes":[{"name":"frame","hits":1,"total_ms":5.000,"self_ms":2.000,"#,
                r#""children":[{"name":"sort","hits":1,"total_ms":3.000,"self_ms":3.000,"#,
                r#""children":[]}]}]}"#,
                "\n"
            )
        );
    }

    #[test]
    fn names_are_escaped_as_json_strings() {
        let profiler = millisecond_profiler();
        profiler.record(vec!["say \"hi\"\\"], 1_000, 1, 1);
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();

        let reply = ask(&mut server, &profiler, "snapshot");

        assert!(reply.contains(r#""name":"say \"hi\"\\""#), "{reply}");
    }

    #[test]
    fn an_unknown_request_replies_with_an_error() {
        let profiler = millisecond_profiler();
        let mut server = ProfileServer::bind("127.0.0.1:0").unwrap();

        assert_eq!(
            ask(&mut server, &profiler, "flamegraph"),
            concat!(
                r#"{"error":"unknown request \"flamegraph\"; try snapshot, recent or series"}"#,
                "\n"
            )
        );
    }
}
