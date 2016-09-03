use std::rc::Rc;
use std::cell::RefCell;
use std::io::{Read, Write, Result, Error, ErrorKind};

use mio::{EventLoop, Token, EventSet, PollOpt};
use mio::tcp::TcpStream;
use toml::Table;

use config;
use encrypt::Encryptor;
use common::parse_header;
use util::{address2str, get_basic_events};
use relay::{Relay, Processor, ProcessResult};
use network::pair2socket_addr;
use asyncdns::{Caller, DNSResolver};


const BUF_SIZE: usize = 32 * 1024;
// SOCKS method definition
const METHOD_NOAUTH: u8 = 0;
// SOCKS command definition
const CMD_CONNECT: u8 = 1;
const _CMD_BIND: u8 = 2;
const CMD_UDP_ASSOCIATE: u8 = 3;


#[derive(Debug, PartialEq)]
enum CheckAuthResult {
    Success,
    BadSocksHeader,
    NoAcceptableMethods,
}
// for each opening port, we have a TCP Relay
// for each connection, we have a TCP Relay Handler to handle the connection
//
// for each handler, we have 2 sockets:
//    local:   connected to the client
//    remote:  connected to remote server

// for each handler, it could be at one of several stages:
#[derive(Debug, PartialEq)]
enum HandleStage {
    // only sslocal: auth METHOD received from local, reply with selection message
    Init,
    // addr received from local, query DNS for remote
    Addr,
    // only sslocal: UDP assoc
    UDPAssoc,
    // DNS resolved, connect to remote
    DNS,
    // still connecting, more data from local received
    Connecting,
    // remote connected, piping local and remote
    Stream,
    Destroyed,
}

pub struct TCPProcessor {
    conf: Rc<Table>,
    stage: HandleStage,
    dns_resolver: Rc<RefCell<DNSResolver>>,
    is_client: bool,
    local_token: Option<Token>,
    local_sock: Option<TcpStream>,
    remote_token: Option<Token>,
    remote_sock: Option<TcpStream>,
    data_to_write_to_local: Vec<u8>,
    data_to_write_to_remote: Vec<u8>,
    client_address: Option<(String, u16)>,
    server_address: Option<(String, u16)>,
    encryptor: Encryptor,
}

macro_rules! need_destroy {
    ($this:expr) => (
        {
            let local_token = $this.local_token.unwrap();
            match $this.remote_token {
                Some(remote_token) => ProcessResult::Failed(vec![local_token, remote_token]),
                _ => ProcessResult::Failed(vec![local_token])
            }
        }
    );
}

macro_rules! try_process {
    ($process:expr) => (
        match $process {
            ProcessResult::Success => {},
            res @ _ => return res,
        }
    );
}


impl TCPProcessor {
    pub fn new(conf: Rc<Table>,
               local_sock: TcpStream,
               dns_resolver: Rc<RefCell<DNSResolver>>,
               is_client: bool)
               -> TCPProcessor {

        let encryptor = Encryptor::new(config::get_str(&conf, "password"));
        let stage = if is_client {
            HandleStage::Init
        } else {
            HandleStage::Addr
        };

        let mut client_address = None;
        if let Ok(addr) = local_sock.peer_addr() {
            client_address = Some((format!("{}", addr.ip()), addr.port()));
        };

        local_sock.set_nodelay(true).ok();

        TCPProcessor {
            conf: conf,
            stage: stage,
            dns_resolver: dns_resolver,
            is_client: is_client,
            local_token: None,
            local_sock: Some(local_sock),
            remote_token: None,
            remote_sock: None,
            data_to_write_to_local: Vec::new(),
            data_to_write_to_remote: Vec::new(),
            client_address: client_address,
            server_address: None,
            encryptor: encryptor,
        }
    }

    pub fn set_remote_token(&mut self, token: Token) {
        self.remote_token = Some(token);
    }

    pub fn add_to_loop(&mut self,
                       token: Token,
                       event_loop: &mut EventLoop<Relay>,
                       events: EventSet,
                       is_local_sock: bool)
                       -> Option<()> {
        let mut sock = if is_local_sock {
            self.local_token = Some(token);
            &mut self.local_sock
        } else {
            self.remote_token = Some(token);
            &mut self.remote_sock
        };

        let result = match sock {
            &mut Some(ref mut sock) => {
                match event_loop.register(sock, token, events, PollOpt::level()) {
                    Ok(_) => Some(()),
                    Err(e) => {
                        error!("register failed because {}", e);
                        None
                    }
                }
            }
            _ => {
                error!("register failed because sock not init");
                None
            }
        };

        match result {
            Some(_) => {
                if is_local_sock {
                    debug!("local socket {:?} has registred events {:?} to event loop", events, token);
                } else {
                    debug!("remote socket {:?} has registred events {:?} to event loop", events, token);
                }
            }
            None => {
                if is_local_sock {
                    error!("failed to register events {:?} for local socket {:?}", events, token);
                } else {
                    error!("failed to register events {:?} for remote socket {:?}", events, token);
                }
            }
        }

        result
    }

    fn change_status(&mut self, event_loop: &mut EventLoop<Relay>, events: EventSet, is_local_sock: bool) {
        let token = if is_local_sock {
            self.local_token
        } else {
            self.remote_token
        };

        debug!("change listened events to {:?}", events);
        self.add_to_loop(token.unwrap(), event_loop, events, is_local_sock);
    }

    fn change_to_writable(&mut self, event_loop: &mut EventLoop<Relay>, is_local_sock: bool) {
        self.change_status(event_loop,
                           get_basic_events() | EventSet::writable() | EventSet::hup(),
                           is_local_sock);
    }

    fn change_to_readable(&mut self, event_loop: &mut EventLoop<Relay>, is_local_sock: bool) {
        self.change_status(event_loop, get_basic_events() | EventSet::hup(), is_local_sock);
    }

    fn receive_data(&mut self, event_loop: &mut EventLoop<Relay>, is_local_sock: bool) -> Option<Vec<u8>> {
        let mut need_destroy = false;
        let mut buf = [0u8; BUF_SIZE];

        macro_rules! read_data {
            ($sock:expr) => (
                match $sock {
                    &mut Some(ref mut sock) => {
                        match sock.read(&mut buf) {
                            Ok(len) => {
                                if (self.is_client && !is_local_sock) || (!self.is_client && is_local_sock) {
                                    match self.encryptor.decrypt(&buf[..len]) {
                                        data @ Some(_) => data,
                                        None => {
                                            info!("cannot decrypt data, maybe a error client");
                                            need_destroy = true;
                                            None
                                        }
                                    }
                                } else {
                                    Some(buf[..len].to_vec())
                                }
                            }
                            Err(e) => {
                                // TODO: consider destroy processor
                                if is_local_sock {
                                    error!("read data from local socket failed because {}", e);
                                } else {
                                    error!("read data from remote socket failed because {}", e);
                                }
                                None
                            }
                        }
                    }
                    _ => {
                        error!("socket is not initialize");
                        None
                    }
                }
            );
        }

        let data = if is_local_sock {
            read_data!(&mut self.local_sock)
        } else {
            read_data!(&mut self.remote_sock)
        };

        if need_destroy {
            self.destroy(event_loop);
        }

        data
    }

    fn send_buf_data(&mut self, event_loop: &mut EventLoop<Relay>, is_local_sock: bool) -> ProcessResult<Vec<Token>> {
        let data;
        if is_local_sock {
            data = self.data_to_write_to_local.clone();
            self.data_to_write_to_local.clear();
        } else {
            data = self.data_to_write_to_remote.clone();
            self.data_to_write_to_remote.clear();
        };

        self.write_to_sock(event_loop, &data, is_local_sock)
    }

    fn write_to_sock(&mut self,
                     event_loop: &mut EventLoop<Relay>,
                     data: &[u8],
                     is_local_sock: bool)
                     -> ProcessResult<Vec<Token>> {
        if data.len() == 0 {
            return ProcessResult::Success;
        }

        let data = if (self.is_client && !is_local_sock) || (!self.is_client && is_local_sock) {
            match self.encryptor.encrypt(data) {
                Some(data) => data,
                _ => {
                    error!("encrypt data failed");
                    return need_destroy!(self);
                }
            }
        } else {
            data.to_vec()
        };

        let (any_error, uncomplete_len) = {
            let mut sock = if is_local_sock {
                &mut self.local_sock
            } else {
                &mut self.remote_sock
            };

            match sock {
                &mut Some(ref mut sock) => {
                    match sock.write(&data) {
                        Ok(size) => {
                            if is_local_sock {
                                debug!("writed {} bytes to local socket", size);
                            } else {
                                debug!("writed {} bytes to remote socket", size);
                            }
                            (false, data.len() - size)
                        }
                        Err(e) => {
                            if is_local_sock {
                                error!("write to local socket error: {}", e);
                            } else {
                                error!("write to remote socket error: {}", e);
                            }
                            (true, data.len())
                        }
                    }
                }
                _ => (false, data.len())
            }
        };

        if any_error {
            return need_destroy!(self);
        } else if uncomplete_len > 0 {
            let offset = data.len() - uncomplete_len;
            let remain = &data[offset..];
            if is_local_sock {
                self.data_to_write_to_local.extend_from_slice(remain);
            } else {
                self.data_to_write_to_remote.extend_from_slice(remain);
            }
            self.change_to_writable(event_loop, is_local_sock);
        }

        ProcessResult::Success
    }

    fn handle_stage_stream(&mut self, event_loop: &mut EventLoop<Relay>, data: &[u8]) -> ProcessResult<Vec<Token>> {
        debug!("handle stage stream");
        self.write_to_sock(event_loop, data, false)
    }

    fn handle_stage_connecting(&mut self, _event_loop: &mut EventLoop<Relay>, data: &[u8]) -> ProcessResult<Vec<Token>> {
        debug!("handle stage connecting");
        self.data_to_write_to_remote.extend_from_slice(data);

        ProcessResult::Success
    }

    fn handle_stage_addr(&mut self, event_loop: &mut EventLoop<Relay>, data: &[u8]) -> ProcessResult<Vec<Token>> {
        debug!("handle stage addr");
        let data = if self.is_client {
            match data[1] {
                CMD_UDP_ASSOCIATE => {
                    self.stage = HandleStage::UDPAssoc;
                    unimplemented!();
                }
                CMD_CONNECT => {
                    &data[3..]
                }
                cmd => {
                    error!("unknown socks command: {}", cmd);
                    return need_destroy!(self);
                }
            }
        } else {
            data
        };

        match parse_header(data) {
            Some((_addr_type, remote_address, remote_port, header_length)) => {
                self.stage = HandleStage::DNS;
                self.server_address = Some((remote_address.clone(), remote_port));
                info!("connecting {} from {}", address2str(&self.server_address), address2str(&self.client_address));

                if self.is_client {
                    let response = &[0x05, 0x00, 0x00, 0x01,
                                     0x00, 0x00, 0x00, 0x00,
                                     0x10, 0x10];
                    try_process!(self.write_to_sock(event_loop, response, true));
                    self.data_to_write_to_remote.extend_from_slice(data);
                    // TODO: change to configuable
                    self.dns_resolver.borrow_mut().resolve(event_loop, "127.0.0.1".to_string(), self.remote_token.unwrap());
                } else {
                    if data.len() > header_length {
                        self.data_to_write_to_remote.extend_from_slice(&data[header_length..]);
                    }
                    self.dns_resolver.borrow_mut().resolve(event_loop, remote_address, self.remote_token.unwrap());
                }
            }
            None => {
                error!("can not parse socks header");
                return need_destroy!(self);
            }
        }

        ProcessResult::Success
    }

    fn handle_stage_init(&mut self, event_loop: &mut EventLoop<Relay>, data: &[u8]) -> ProcessResult<Vec<Token>> {
        debug!("handle stage init");
        match self.check_auth_method(data) {
            CheckAuthResult::Success => {
                try_process!(self.write_to_sock(event_loop, &[0x05, 0x00], true));
                self.stage = HandleStage::Addr;
                return ProcessResult::Success;
            }
            CheckAuthResult::BadSocksHeader => { }
            CheckAuthResult::NoAcceptableMethods => {
                self.write_to_sock(event_loop, &[0x05, 0xff], true);
            }
        }

        need_destroy!(self)
    }

    fn check_auth_method(&self, data: &[u8]) -> CheckAuthResult {
        if data.len() < 3 {
            warn!("method selection header too short");
            return CheckAuthResult::BadSocksHeader;
        }

        let socks_version = data[0];
        if socks_version != 5 {
            warn!("unsupported SOCKS protocol version {}", socks_version);
            return CheckAuthResult::BadSocksHeader;
        }

        let nmethods = data[1];
        if nmethods < 1 || data.len() as u8 != nmethods + 2 {
            warn!("NMETHODS and number of METHODS mismatch");
            return CheckAuthResult::BadSocksHeader;
        }

        let mut noauto_exist = false;
        for method in &data[2..] {
            if *method == METHOD_NOAUTH {
                noauto_exist = true;
                break;
            }
        }

        if !noauto_exist {
            warn!("none of SOCKS METHOD's requested by client is supported");
            return CheckAuthResult::NoAcceptableMethods;
        }

        return CheckAuthResult::Success;
    }

    fn on_local_read(&mut self, event_loop: &mut EventLoop<Relay>) -> ProcessResult<Vec<Token>> {
        match self.receive_data(event_loop, true) {
            Some(data) => {
                if data.len() > 0 {
                    match self.stage {
                        HandleStage::Init => {
                            self.handle_stage_init(event_loop, &data)
                        }
                        HandleStage::Addr => {
                            self.handle_stage_addr(event_loop, &data)
                        }
                        HandleStage::Connecting => {
                            self.handle_stage_connecting(event_loop, &data)
                        }
                        HandleStage::Stream => {
                            self.handle_stage_stream(event_loop, &data)
                        }
                        _ => ProcessResult::Success
                    }
                } else {
                    need_destroy!(self)
                }
            }
            _ => ProcessResult::Success
        }
    }

    fn on_remote_read(&mut self, event_loop: &mut EventLoop<Relay>) -> ProcessResult<Vec<Token>> {
        match self.receive_data(event_loop, false) {
            Some(data) => {
                if data.len() > 0 {
                    self.write_to_sock(event_loop, &data, true)
                } else {
                    need_destroy!(self)
                }
            }
            _ => ProcessResult::Success
        }
    }

    fn on_local_write(&mut self, event_loop: &mut EventLoop<Relay>) -> ProcessResult<Vec<Token>> {
        let result = if self.data_to_write_to_local.len() > 0 {
            self.send_buf_data(event_loop, true)
        } else {
            ProcessResult::Success
        };

        if self.data_to_write_to_local.len() == 0 {
            self.change_to_readable(event_loop, true);
        }

        result
    }

    fn on_remote_write(&mut self, event_loop: &mut EventLoop<Relay>) -> ProcessResult<Vec<Token>> {
        self.stage = HandleStage::Stream;

        let result = if self.data_to_write_to_remote.len() > 0 {
            self.send_buf_data(event_loop, false)
        } else {
            ProcessResult::Success
        };

        if self.data_to_write_to_remote.len() == 0 {
            self.change_to_readable(event_loop, false);
        }

        result
    }

    fn create_connection(&mut self, ip: &str, port: u16) -> Result<TcpStream> {
        match pair2socket_addr(ip, port) {
            Ok(addr) => {
                TcpStream::connect(&addr).map(|sock| {
                    sock.set_nodelay(true).ok();
                    sock
                })
            }
            Err(e) => {
                Err(Error::new(ErrorKind::InvalidData, e))
            }
        }
    }
}

impl Caller for TCPProcessor {
    fn handle_dns_resolved(&mut self, event_loop: &mut EventLoop<Relay>, hostname_ip: Option<(String, String)>, errmsg: Option<&str>) -> ProcessResult<Vec<Token>> {
        debug!("handle_dns_resolved: {:?}", hostname_ip);
        if errmsg.is_some() {
            info!("resolve DNS error: {}", errmsg.unwrap());
            return need_destroy!(self);
        }

        match hostname_ip {
            Some((_hostname, ip)) => {
                self.stage = HandleStage::Connecting;
                let port = if self.is_client {
                    // TODO: change to select a server
                    config::get_i64(&self.conf, "remote_port") as u16
                } else {
                    match self.server_address {
                        Some((_, port)) => port,
                        _ => return need_destroy!(self),
                    }
                };

                self.remote_sock = match self.create_connection(&ip, port) {
                    Ok(sock) => {
                        debug!("created remote socket to {}:{}", ip, port);
                        Some(sock)
                    }
                    Err(e) => {
                        error!("create remote socket for connecting to {} failed: {}", ip, e);
                        return need_destroy!(self);
                    }
                };

                let token = self.remote_token;
                self.add_to_loop(token.unwrap(),
                                 event_loop,
                                 get_basic_events() | EventSet::writable() | EventSet::hup(),
                                 false);
            }
            _ => {
                return need_destroy!(self);
            }
        }

        ProcessResult::Success
    }
}

impl Processor for TCPProcessor {
    fn process(&mut self, event_loop: &mut EventLoop<Relay>, token: Token, events: EventSet) -> ProcessResult<Vec<Token>> {
        debug!("current handle stage is {:?}", self.stage);
        if Some(token) == self.local_token {
            debug!("got events for local socket {:?}: {:?}", token, events);
            if events.is_error() {
                error!("got events error from local socket on TCPProcessor");
                return need_destroy!(self);
            }

            if events.is_readable() || events.is_hup() {
                try_process!(self.on_local_read(event_loop));
            }

            if events.is_writable() {
                try_process!(self.on_local_write(event_loop));
            }
        } else if Some(token) == self.remote_token {
            debug!("got events for remote socket {:?}: {:?}", token, events);
            if events.is_error() {
                error!("got events error from remote socket on TCPProcessor");
                return need_destroy!(self);
            }

            if events.is_readable() || events.is_hup() {
                try_process!(self.on_remote_read(event_loop));
            }

            if events.is_writable() {
                try_process!(self.on_remote_write(event_loop));
            }
        }

        ProcessResult::Success
    }

    fn destroy(&mut self, event_loop: &mut EventLoop<Relay>) {
        if self.stage == HandleStage::Destroyed {
            return;
        }

        debug!("destroy processor: {:?}, {:?}", self.local_token, self.remote_token);
        self.stage = HandleStage::Destroyed;

        let sock = &self.local_sock.take().unwrap();
        event_loop.deregister(sock).ok();

        if self.remote_sock.is_some() {
            let sock = &self.remote_sock.take().unwrap();
            event_loop.deregister(sock).ok();
        }

        if self.remote_token.is_some() {
            let token = self.remote_token.unwrap();
            self.dns_resolver.borrow_mut().remove_caller(token);
        }
    }
}
