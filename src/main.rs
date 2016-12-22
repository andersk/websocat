#![recursion_limit = "1024"] // error_chain
 
extern crate websocket;
extern crate env_logger;
#[macro_use]
extern crate log;
#[macro_use]
extern crate error_chain;
extern crate url;
#[macro_use] // crate_version
extern crate clap;

#[cfg(feature = "unix_socket")]
extern crate unix_socket;

const BUFSIZ : usize = 8192;

use std::thread;
use std::io::{stdin,stdout};

use websocket::{Message, Sender, Receiver, DataFrame, Server as WsServer};
use websocket::message::Type;
use websocket::client::request::Url;
use websocket::Client;

use std::borrow::Borrow;
use std::io::{Error as IoError, ErrorKind as IoErrorKind, Write, Read};

error_chain! {
    foreign_links {
        ::std::io::Error, Io;
        log::SetLoggerError, Log;
        ::url::ParseError, Url;
        ::websocket::result::WebSocketError, Ws;
        ::std::env::VarError, Ev;
    }
    errors {
        InvalidSpecifier(t : String) {
            description("invalid specifier")
            display("Invalid client or server specifier `{}`", t)
        }
    }
}

// Initialize logger with default "info" log level:
fn init_logger() -> Result<()> {
    let mut builder = env_logger::LogBuilder::new();
    builder.filter(None, log::LogLevelFilter::Info);
    if ::std::env::var("RUST_LOG").is_ok() {
       builder.parse(&::std::env::var("RUST_LOG")?);
    }
    builder.init()?;
    Ok(())
}

#[derive(Copy,Clone)]
enum WebSocketMessageMode {
    Binary,
    Text,
}

struct SenderWrapper<T: Sender> (T, WebSocketMessageMode);

impl<T: Sender> ::std::io::Write for SenderWrapper<T> {
    fn write(&mut self, buf: &[u8]) -> ::std::io::Result<usize> {
        let ret;
        let len = buf.len();
        if len > 0 {
            debug!("Sending message of {} bytes", len);
            match self.1 {
                WebSocketMessageMode::Binary => {
                    let message = Message::binary(buf);
                    ret = self.0.send_message(&message)
                }
                WebSocketMessageMode::Text => {
                    let text_tmp;
                    let text = match ::std::str::from_utf8(buf) {
                        Ok(x) => x,
                        Err(_) => {
                            error!("Invalid UTF-8 in --text mode. Sending lossy data. May be caused by unlucky buffer splits.");
                            text_tmp = String::from_utf8_lossy(buf);
                            text_tmp.as_ref()
                        }
                    };
                    let message = Message::text(text);
                    ret = self.0.send_message(&message);
                }
            }
        } else {
            // Interpret zero length buffer is request
            // to close communication
            
            debug!("Sending the closing message");
            ret = self.0.send_message(&Message::close());
        }
        ret.map_err(|e|IoError::new(IoErrorKind::BrokenPipe, e))?;
        Ok(len)
    }
    fn flush(&mut self) -> ::std::io::Result<()> {
        Ok(())
    }
}

struct ReceiverWrapper<T: Receiver<DataFrame>> (T);

impl<T:Receiver<DataFrame>> ::std::io::Read for ReceiverWrapper<T> {
    fn read(&mut self, buf: &mut [u8]) -> ::std::io::Result<usize> {
        let ret = self.0.recv_message();
        let msg : Message = ret.map_err(|e|IoError::new(IoErrorKind::BrokenPipe, e))?;
        
        match msg.opcode {
            Type::Close => {
                Ok(0)
            }
            Type::Ping => {
                // Sender used to be in a separate thread with a channel
                // now there's no channel, so trickier to combine ping replies
                // and usual data exchange
                error!("Received ping, but replying to pings is not implemented");
                error!("Open an issue if you want ping replies in websocat");
                Ok(0)
            }
            _ => {
                let msgpayload : &[u8] = msg.payload.borrow();
                let len = msgpayload.len();
                debug!("Received message of {} bytes", len);
                
                assert!(buf.len() >= len);
                
                buf[0..len].copy_from_slice(msgpayload);
                
                Ok(len)
            }
        }
    }
}

struct Endpoint<R, W>
    where R : Read + Send + 'static, W: Write + Send + 'static
{
    reader: R,
    writer: W,
}

type IEndpoint = Endpoint<Box<Read+Send>, Box<Write+Send>>;

struct DataExchangeSession<R1, R2, W1, W2> 
    where R1 : Read  + Send + 'static, 
          R2 : Read  + Send + 'static,
          W1 : Write + Send + 'static,
          W2 : Write + Send + 'static,
{
    endpoint1: Endpoint<R1, W1>,
    endpoint2: Endpoint<R2, W2>,
}

// Derived from https://doc.rust-lang.org/src/std/up/src/libstd/io/util.rs.html#46-61
pub fn copy_with_flushes<R: ?Sized, W: ?Sized>(reader: &mut R, writer: &mut W) -> ::std::io::Result<u64>
    where R: Read, W: Write
{
    let mut buf = [0; BUFSIZ];
    let mut written = 0;
    loop {
        let len = match reader.read(&mut buf) {
            Ok(0) => return Ok(written),
            Ok(len) => len,
            Err(ref e) if e.kind() == IoErrorKind::Interrupted => continue,
            Err(ref e) if e.kind() == IoErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..len])?;
        writer.flush()?;
        written += len as u64;
    }
}

impl<R1,R2,W1,W2> DataExchangeSession<R1,R2,W1,W2> 
    where R1 : Read  + Send + 'static,
          R2 : Read  + Send + 'static, 
          W1 : Write + Send + 'static,
          W2 : Write + Send + 'static,
{
    fn data_exchange(self) -> Result<()> {
    
        let mut reader1 = self.endpoint1.reader;
        let mut writer1 = self.endpoint1.writer;
        let mut reader2 = self.endpoint2.reader;
        let mut writer2 = self.endpoint2.writer;
    
        let receive_loop = thread::Builder::new().spawn(move || -> Result<()> {
            // Actual data transfer happens here
            copy_with_flushes(&mut reader1, &mut writer2)?;
            writer2.write(b"")?; // signal close
            Ok(())
        })?;
    
        // Actual data transfer happens here
        copy_with_flushes(&mut reader2, &mut writer1)?;
        writer1.write(b"")?; // Signal close
    
        debug!("Waiting for receiver side to exit");
    
        receive_loop.join().map_err(|x|format!("{:?}",x))?
    }
}

fn get_websocket_endpoint(urlstr: &str, wsm : WebSocketMessageMode) -> Result<
        Endpoint<
            ReceiverWrapper<websocket::client::Receiver<websocket::WebSocketStream>>,
            SenderWrapper<websocket::client::Sender<websocket::WebSocketStream>>>
        > {
    let url = Url::parse(urlstr)?;

    info!("Connecting to {}", url);

    let mut request = Client::connect(url)?;
    
    request.headers.set(
        ::websocket::header::WebSocketProtocol(
            vec!["binary".to_string()]
        )
    );

    let response = request.send()?; // Send the request and retrieve a response

    info!("Validating response...");

    response.validate()?; // Validate the response

    info!("Successfully connected");

    let (sender, receiver) = response.begin().split();
    
    let endpoint = Endpoint {
        reader : ReceiverWrapper(receiver),
        writer : SenderWrapper(sender, wsm),
    };
    Ok(endpoint)
}

fn get_tcp_endpoint(addr: &str) -> Result<
        Endpoint<
            ::std::net::TcpStream,
            ::std::net::TcpStream,
        >> {
    let sock = ::std::net::TcpStream::connect(addr)?;

    let endpoint = Endpoint {
        reader : sock.try_clone()?,
        writer : sock.try_clone()?,
    };
    info!("Connected to TCP {}", addr);
    Ok(endpoint)
}

#[cfg(feature = "unix_socket")]
fn get_unix_socket_address(addr: &str, abstract_: bool) -> String {
    if abstract_ {
        "\x00".to_string() + addr
    } else {
        addr.to_string()
    }
}

#[cfg(feature = "unix_socket")]
fn get_unix_socket_endpoint(addr: &str) -> Result<
        Endpoint<
            ::unix_socket::UnixStream,
            ::unix_socket::UnixStream,
        >> {
    let sock = ::unix_socket::UnixStream::connect(addr)?;

    let endpoint = Endpoint {
        reader : sock.try_clone()?,
        writer : sock.try_clone()?,
    };
    info!("Connected to UNIX socket {}", addr);
    Ok(endpoint)
}

fn get_stdio_endpoint() -> Result<Endpoint<std::io::Stdin, std::io::Stdout>> {
    Ok(
        Endpoint {
            reader : stdin(),
            writer : stdout(),
        }
    )
}

fn get_forkexec_endpoint(cmdline: &str, shell: bool) 
        -> Result<Endpoint<std::process::ChildStdout, std::process::ChildStdin>> {
    
    let mut cmdbuf;
    let cmd = if shell {
        cmdbuf = std::process::Command::new("sh");
        cmdbuf.args(&["-c", cmdline])
    } else {
        cmdbuf = std::process::Command::new(cmdline);
        &mut cmdbuf
    };
    
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;
        
    Ok(
        Endpoint {
            reader : child.stdout.take().unwrap(),
            writer : child.stdin.take().unwrap(),
        }
    )
}





struct TcpServer(::std::net::TcpListener);

impl TcpServer {
    fn new(addr: &str) -> Result<Self> {
        Ok(TcpServer(::std::net::TcpListener::bind(addr)?))
    }
}

impl Server for TcpServer {    
    fn accept_client(&mut self) -> Result<IEndpoint> {
        let (sock, addr) = self.0.accept()?;
        info!("TCP client connection from {}", addr);
        let endpoint = Endpoint {
            reader : sock.try_clone()?,
            writer : sock.try_clone()?,
        };
        Ok(endpoint.upcast())
    }
}

#[cfg(feature = "unix_socket")]
struct UnixSocketServer(::unix_socket::UnixListener);

#[cfg(feature = "unix_socket")]
impl UnixSocketServer {
    fn new(addr: &str) -> Result<Self> {
        Ok(UnixSocketServer(::unix_socket::UnixListener::bind(addr)?))
    }
}

#[cfg(feature = "unix_socket")]
impl Server for UnixSocketServer {    
    fn accept_client(&mut self) -> Result<IEndpoint> {
        let (sock, addr) = self.0.accept()?;
        info!("UNIX client connection from {:?}", addr);
        let endpoint = Endpoint {
            reader : sock.try_clone()?,
            writer : sock.try_clone()?,
        };
        Ok(endpoint.upcast())
    }
}



struct WebsockServer<'a>(WsServer<'a>, WebSocketMessageMode);

impl<'a> WebsockServer<'a> {
    fn new(addr: &str, wsm:WebSocketMessageMode) -> Result<Self> {
        Ok(WebsockServer(WsServer::bind(addr)?, wsm))
    }
}

impl<'a> Server for WebsockServer<'a> {    
    fn accept_client(&mut self) -> Result<IEndpoint> {
        let connection = self.0.accept()?;
        info!("WebSocket client connection ...");
        let request = connection.read_request()?;
        request.validate()?;
        let response = request.accept(); // Form a response
        let mut client = response.send()?; // Send the response

        let ip = client.get_mut_sender()
            .get_mut()
            .peer_addr()
            .unwrap();

        info!("... from IP {}", ip);

        let (sender, receiver) = client.split();

        let endpoint = Endpoint {
            reader : ReceiverWrapper(receiver),
            writer : SenderWrapper(sender, self.1),
        };
        Ok(endpoint.upcast())
    }
}






impl<R,W> Endpoint<R,W> 
    where R : Read + Send + 'static, W: Write + Send + 'static
{
    fn upcast(self) -> IEndpoint  {
        Endpoint {
            reader: Box::new(self.reader) as Box<Read +Send>,
            writer: Box::new(self.writer) as Box<Write+Send>,
        }
    }
}


trait Server
{
    fn accept_client(&mut self) -> Result<IEndpoint>;
    
    fn start_serving(&mut self, spec2: &str, once: bool, wsm:WebSocketMessageMode) -> Result<()> {
        let spec2s = spec2.to_string();
        let closure = move |endpoint, spec2 : String|{
            let spec2_ = get_endpoint_by_spec(spec2.as_str(), wsm)?;
            let endpoint2 = match spec2_ {
                Spec::Server(mut x) => {
                    x.accept_client()?
                }
                Spec::Client(p1) => {
                    p1
                }
            };
            let des = DataExchangeSession {
                endpoint1 : endpoint,
                endpoint2 : endpoint2,
            };
            
            des.data_exchange()
        };
        if once {
            let endpoint = self.accept_client()?;
            closure(endpoint, spec2s)
        } else {
            let cl2 = ::std::sync::Arc::new(closure);
            loop {
                let ret = self.accept_client();
                let endpoint = match ret {
                    Ok(x) => x,
                    Err(er) => {
                        warn!("Can't accept client: {}", er);
                        continue;
                    }
                };
                let cl3 = cl2.clone();
                let spec2s2 = spec2s.clone();
                if let Err(x) = thread::Builder::new().spawn(move|| {
                    if let Err(x) = cl3(endpoint, spec2s2) {
                        warn!("Error while serving: {}", x);
                    }
                }) {
                    warn!("Error creating thread: {}", x);
                    thread::sleep(::std::time::Duration::from_millis(200));
                }
            }
        }
    }
    
    fn upcast(self) -> Box<Server+Send> 
        where Self : Sized + Send + 'static
        { Box::new(self) as Box<Server+Send> }
}

fn main2(spec1: &str, spec2: &str, once: bool, wsm: WebSocketMessageMode) -> Result<()> {
    let spec1_ = get_endpoint_by_spec(spec1, wsm)?;
    
    match spec1_ {
        Spec::Server(mut x) => {
            x.start_serving(spec2, once, wsm)
        }
        Spec::Client(p1) => {
            let spec2_ = get_endpoint_by_spec(spec2, wsm)?;
            
            let otherendpoint = match spec2_ {
                Spec::Server(mut x) => {
                    let t = x.accept_client()?;
                    t
                }
                Spec::Client(p2) => {
                    p2
                }
            };
            
            let des = DataExchangeSession {
                endpoint1 : p1,
                endpoint2 : otherendpoint,
            };
            
            des.data_exchange()
        }
    }
}

enum Spec {
    Server(Box<Server + Send>),
    Client(IEndpoint)
}

fn get_endpoint_by_spec(specifier: &str, wsm: WebSocketMessageMode) -> Result<Spec> {
    use Spec::{Server,Client};
    match specifier {
        x if x == "-"               =>
                Ok(Client(get_stdio_endpoint()?.upcast())),
        x if x.starts_with("ws:")   => 
                Ok(Client(get_websocket_endpoint(x,wsm)?.upcast())),
        x if x.starts_with("wss:")  => 
                Ok(Client(get_websocket_endpoint(x,wsm)?.upcast())),
        x if x.starts_with("tcp:")  => 
                Ok(Client(get_tcp_endpoint(&x[4..])?.upcast())),
        
        #[cfg(feature = "unix_socket")]
        x if x.starts_with("unix:")  => 
                Ok(Client(get_unix_socket_endpoint(&get_unix_socket_address(&x[5..], false))?.upcast())),
        #[cfg(feature = "unix_socket")]
        x if x.starts_with("abstract:")  => 
                Ok(Client(get_unix_socket_endpoint(&get_unix_socket_address(&x[9..], true))?.upcast())),
        #[cfg(feature = "unix_socket")]
        x if x.starts_with("l-unix:")  => 
                Ok(Server(UnixSocketServer::new(&get_unix_socket_address(&x[7..], false))?.upcast())),
        #[cfg(feature = "unix_socket")]
        x if x.starts_with("l-abstract:")  => 
                Ok(Server(UnixSocketServer::new(&get_unix_socket_address(&x[11..], true))?.upcast())),
        
        #[cfg(not(feature = "unix_socket"))]
        x if x.starts_with("unix:")  => 
                Err("UNIX socket support not compiled in".into()),
        #[cfg(not(feature = "unix_socket"))]
        x if x.starts_with("abstract:")  => 
                Err("UNIX socket support not compiled in".into()),
        #[cfg(not(feature = "unix_socket"))]
        x if x.starts_with("l-unix:")  => 
                Err("UNIX socket support not compiled in".into()),
        #[cfg(not(feature = "unix_socket"))]
        x if x.starts_with("l-abstract:")  => 
                Err("UNIX socket support not compiled in".into()),
        
        x if x.starts_with("l-tcp:")  => 
                Ok(Server(TcpServer::new(&x[6..])?.upcast())),
        x if x.starts_with("l-ws:")  => 
                Ok(Server(WebsockServer::new(&x[5..], wsm)?.upcast())),
        x if x.starts_with("exec:")  => 
                Ok(Client(get_forkexec_endpoint(&x[5..], false)?.upcast())),
        x if x.starts_with("sh-c:")  => 
                Ok(Client(get_forkexec_endpoint(&x[5..], true)?.upcast())),
        x => Err(ErrorKind::InvalidSpecifier(x.to_string()).into()),
    }
}

fn try_main() -> Result<()> {
    //env_logger::init()?;
    init_logger()?;

    // setup command line arguments
    let matches = ::clap::App::new("websocat")
        .version(crate_version!())
        .author("Vitaly \"_Vi\" Shukela <vi0oss@gmail.com>")
        .about("Exchange binary data between binary websocket and something.\nSocat analogue with websockets.")
        .arg(::clap::Arg::with_name("spec1")
             .help("First specifier.")
             .required(true)
             .index(1))
        .arg(::clap::Arg::with_name("spec2")
             .help("Second specifier.")
             .required(true)
             .index(2))
        .arg(::clap::Arg::with_name("text")
             .help("Send WebSocket text messages instead of binary (unstable)")
             .required(false)
             .short("-t")
             .long("--text"))
        .after_help(r#"
Specifiers can be:
  ws[s]://<rest of websocket URL>   Connect to websocket
  tcp:host:port                     Connect to TCP
  unix:path                         Connect to UNIX socket
  abstract:addr                     Connect to abstract UNIX socket
  l-ws:host:port                    Listen unencrypted websocket
  l-tcp:host:port                   Listen TCP connections
  l-unix:path                       Listen for UNIX socket connections on path
  l-abstract:addr                   Listen for UNIX socket connections on abstract address
  -                                 stdin/stdout
  exec:program                      spawn a program (no arguments)
  sh-c:program                      execute a command line with 'sh -c'
  (more to be implemented)
  
Examples:
  websocat l-tcp:0.0.0.0:9559 ws://echo.websocket.org/
    Listen port 9959 on address :: and forward 
    all connections to a public loopback websocket
  websocat l-ws:127.0.0.1:7878 tcp:127.0.0.1:1194
    Listen websocket and forward connections to local tcp
    Use nginx proxy for SSL if you want
  websocat - wss://myserver/mysocket
    Connect stdin/stdout to a secure web socket.
    Like netcat, but for websocket.
    `ssh user@host -o ProxyHommand "websocat - ws://..."`
  websocat ws://localhost:1234/ tcp:localhost:1235
    Connect both to websocket and to TCP and exchange data.
    
Specify listening part first, unless you want websocat to serve once.

IPv6 supported, just use specs like `l-ws:::1:4567`

Web socket usage is not obligatory, you can use any specs on both sides.
If you want wss:// server, use socat or nginx in addition.
"#)
        .get_matches();

    let spec1  = matches.value_of("spec1") .ok_or("no listener_spec" )?;
    let spec2 = matches.value_of("spec2").ok_or("no connector_spec")?;
    //
    let wsm = if matches.is_present("text") { 
        WebSocketMessageMode::Text 
    } else {
        WebSocketMessageMode::Binary
    };
    
    main2(spec1, spec2, false, wsm)?;

    debug!("Exited");
    Ok(())
}

fn main() {
    if let Err(x) = try_main() {
        let _ = writeln!(::std::io::stderr(), "{}", x);
    }
}
