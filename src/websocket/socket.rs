use std::io::net::tcp::TcpStream;
use std::io::net::ip::{SocketAddr, Ipv4Addr};
use std::io::net::get_host_addresses;
use std::io::{Buffer, Reader, Writer, IoResult, BufferedStream, standard_error};
use std::io;
use std::collections::TreeMap;
use url::Url;
use openssl::ssl;
use openssl::ssl::{SslStream, SslContext};

#[cfg(test)]
use test::Bencher;
#[cfg(test)]
use serialize::json::ToJson;

use nonce::Nonce;
use message::{WSMessage, WSHeader, WS_FIN, WS_OPCTRL, WS_MASK, WS_LEN, WS_LEN16, WS_LEN64};


pub enum NetworkStream {
    NormalStream(TcpStream),
    SslProtectedStream(SslStream<TcpStream>)
}

impl Reader for NetworkStream {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> {
        match *self {
            NormalStream(ref mut s) => s.read(buf),
            SslProtectedStream(ref mut s) => s.read(buf)
        }
    }
}

impl Writer for NetworkStream {
    fn write(&mut self, buf: &[u8]) -> IoResult<()> {
        match *self {
            NormalStream(ref mut s) => s.write(buf),
            SslProtectedStream(ref mut s) => s.write(buf)
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        match *self {
            NormalStream(ref mut s) => s.flush(),
            SslProtectedStream(ref mut s) => s.flush()
        }
    }
}

pub struct WebSocket<S = NetworkStream> {
    stream: Option<BufferedStream<S>>,
    connected: bool,
    pub remote_addr: Option<SocketAddr>,
    pub url: Url,
    use_ssl: bool,
}

impl WebSocket {
    pub fn new(url: Url) -> IoResult<WebSocket> {
        let addr = match try!(url.domain()
            .map(|h| get_host_addresses(h)
                 .map(|v| v.move_iter().find(|&a| {
                     match a {
                         Ipv4Addr(..) => true,
                         _ => false
                     }
                 })))
            .unwrap_or(Err(standard_error(io::InvalidInput)))) {
                Some(a) => a,
                None => return Err(standard_error(io::FileNotFound))
            };

        let use_ssl = url.scheme.as_slice() == "wss";

        let port = match url.port() {
            Some(p) => p,
            None if use_ssl => 443,
            _ => 80
        };

        Ok(WebSocket {
            stream: None,
            connected: false,
            remote_addr: Some(SocketAddr{ ip: addr, port: port }),
            url: url,
            use_ssl: use_ssl,
        })
    }

    #[allow(unused_variable)]
    fn try_connect(&mut self) -> IoResult<()> {
        let s = try!(self.remote_addr.map(|ref a| TcpStream::connect(format!("{}", a.ip).as_slice(), a.port)).unwrap_or_else(|| Err(standard_error(io::InvalidInput))));
        self.stream = Some(BufferedStream::new(
            if self.use_ssl {
                SslProtectedStream(try!(SslStream::new(&try!(SslContext::new(ssl::Sslv23).map_err(|e| standard_error(io::OtherIoError))), s)
                                        .map_err(|e| standard_error(io::OtherIoError))))
            } else {
                NormalStream(s)
            }));
        Ok(())
    }

    fn send_headers(&mut self, nonce: &str) -> IoResult<()> {
        let s = match self.stream { Some(ref mut s) => s, None => return Err(standard_error(io::NotConnected)) };
        try!(s.write(format!("GET {} HTTP/1.1\r\n", self.url.serialize_path().unwrap_or("/".to_string())).as_bytes()));
        try!(s.write(format!("Host: {}\r\n", self.url.host().unwrap()).as_bytes()));
        try!(s.write("Upgrade: websocket\r\n".as_bytes()));
        try!(s.write("Connection: Upgrade\r\n".as_bytes()));
        try!(s.write(format!("Origin: {}\r\n", self.url.serialize_no_fragment()).as_bytes()));
        try!(s.write("Sec-WebSocket-Protocol: char, superchat\r\n".as_bytes()));
        try!(s.write("Sec-WebSocket-Version: 13\r\n".as_bytes()));
        try!(s.write(format!("Sec-WebSocket-Key: {}\r\n", nonce).as_bytes()));
        try!(s.write("\r\n".as_bytes()));
        s.flush()
    }

    fn read_response(&mut self, nonce: &str) -> IoResult<()> {
        let spaces: &[_] = &[' ', '\t', '\r', '\n'];
        let s = match self.stream { Some(ref mut s) => s, None => return Err(standard_error(io::NotConnected)) };
        let status = try!(s.read_line()).as_slice().splitn(2, ' ').nth(1).and_then(|s| from_str::<uint>(s));

        match status {
            Some(101) => (),
            _ => return Err(standard_error(io::InvalidInput))
        }

        let headers = s.lines().map(|r| r.unwrap_or("\r\n".to_string())) .take_while(|l| l.as_slice() != "\r\n")
            .map(|s| s.as_slice().splitn(1, ':').map(|s| s.trim_chars(spaces).to_string()).collect::<Vec<String>>())
            .map(|p| (p[0].to_string(), p[1].to_string()))
            .collect::<TreeMap<String, String>>();

        try!(s.flush());

        let response = headers.find(&"Sec-WebSocket-Accept".to_string());
        match response {
            Some(r) if nonce == r.as_slice() => (),
            _ => return Err(standard_error(io::InvalidInput))
        }

        Ok(())
    }

    pub fn connect(&mut self) -> IoResult<()> {
        let mut nonce = Nonce::new();

        try!(self.try_connect());
        try!(self.send_headers(nonce.as_slice()));

        nonce = nonce.encode();
        try!(self.read_response(nonce.as_slice()));

        self.connected = true;

        Ok(())
    }

    fn read_header(&mut self) -> IoResult<WSHeader> {
        // XXX: this is a bug, WSHeader should accept u16
        Ok(WSHeader::from_bits_truncate(try!(self.read_be_u16()) as u32))
    }

    fn read_length(&mut self, header: &WSHeader) -> IoResult<uint> {
        let wslen = header & WS_LEN;
        if wslen == WS_LEN16 { self.read_be_u16().map(|v| v as uint) }
        else if wslen == WS_LEN64 { self.read_be_u64().map(|v| v as uint) }
        else { Ok(wslen.bits() as uint) }
    }

    pub fn read_message(&mut self) -> IoResult<WSMessage> {
        let header = try!(self.read_header());
        let len = try!(self.read_length(&header));

        let data = if header.contains(WS_MASK) {
            WebSocket::unmask_data(try!(self.read_exact(len)), try!(self.read_be_u32()))
        } else {
            try!(self.read_exact(len))
        };

        Ok(WSMessage { header: header, data: data })
    }

    fn unmask_data(data: Vec<u8>, mask: u32) -> Vec<u8> {
        data.iter().enumerate().map(|(i, b)| b ^ (mask >> ((i % 4) << 3) & 0xff) as u8).collect::<Vec<u8>>()
    }

    // TODO: send_message(&mut self, &WSMessage) -> IoResult<()>

    pub fn iter(&mut self) -> WSMessages {
        WSMessages { sock: self }
    }
}

impl Reader for WebSocket {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> {
        match self.stream {
            Some(ref mut s) => s.read(buf),
            None => Err(standard_error(io::NotConnected))
        }
    }
}

impl Writer for WebSocket {
    fn write(&mut self, buf: &[u8]) -> IoResult<()> {
        match self.stream {
            Some(ref mut s) => s.write(buf),
            None => Err(standard_error(io::NotConnected))
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        match self.stream {
            Some(ref mut s) => s.flush(),
            None => Err(standard_error(io::NotConnected))
        }
    }
}

impl Buffer for WebSocket {
    fn fill_buf<'a>(&'a mut self) -> IoResult<&'a [u8]> {
        match self.stream {
            Some(ref mut s) => s.fill_buf(),
            None => Err(standard_error(io::NotConnected))
        }
    }

    fn consume(&mut self, amt: uint) {
        match self.stream {
            Some(ref mut s) => s.consume(amt),
            None => ()
        }
    }
}

pub struct WSMessages<'a> {
    sock: &'a mut WebSocket
}

pub struct WSDefragMessages<'a> {
    underlying: &'a mut WSMessages<'a>,
    buffer: WSMessage
}

impl<'a> WSMessages<'a> {
    pub fn defrag(&'a mut self) -> WSDefragMessages<'a> {
        WSDefragMessages{ underlying: self, buffer: WSMessage{ header: WSHeader.empty(), data: Vec::new() } }
    }
}

impl<'a> Iterator<WSMessage> for WSMessages<'a> {
    fn next(&mut self) -> Option<WSMessage> {
        self.sock.read_message().ok()
    }
}

impl<'a> WSDefragMessages<'a> {
    fn popbuf(&mut self) -> Option<WSMessage> {
        if self.buffer.data.is_empty() {
            None
        } else {
            let buf = WSMessage{ header: WSHeader.empty(), data: Vec::new() };
            mem::swap(self.buffer, &mut buf);
            Some(buf)
        }
    }

    fn swapbuf(&mut self, msg: &mut WSMessage) -> WSMessage {
        mem::swap(self.buffer, msg);
        return msg;
    }
}

impl<'a> Iterator<WSMessage> for WSDefragMessages<'a> {
    fn next(&mut self) -> Option<WSMessage> {
        loop {
            match self.underlying.next() {
                None => return self.popbuf(),
                Some(msg) => if msg.header.contains(WS_FIN) {
                    if msg.header & WS_OPCODE == WS_OPCONT {
                        self.buffer.push(msg);
                        return self.popbuf();
                    } else {
                        return Some(msg);
                    }

                } else {
                    if msg.header & WS_OPCODE == WS_OPCONT {
                        self.buffer.push(msg);
                    } else {
                        return self.swapbuf(&mut msg);
                    }
                }
            }
        }
    }
}

#[bench]
#[allow(dead_code)]
fn test_connect(b: &mut Bencher) {
    let url = Url::parse("wss://stream.pushbullet.com/websocket/").unwrap();
    let mut ws = WebSocket::new(url).unwrap();

    match ws.connect() {
        Err(e) => fail!("error: {}", e),
        _ => ()
    }
    let msg = ws.read_message().unwrap();
    println!("received: {} {} {}", msg, msg.to_string(), msg.to_json());
    for msg in ws.iter() {
        println!("{}", msg.to_string());
    }
}