use std::fmt;
use std::net::SocketAddr;

use mio::{Handler, Token, EventSet, EventLoop};

use mode::ServerChooser;
use config::CONFIG;
use network::pair2addr;
use collections::Holder;
use asyncdns::{DnsResolver, Caller, HostIpPair};
use util::{RcCell, new_rc_cell};
use error::{DnsError, SocketError, Result};
use crypto::error::Error as CryptoError;

pub use self::tcp_relay::TcpRelay;
pub use self::udp_relay::UdpRelay;
pub use self::tcp_processor::TcpProcessor;
pub use self::udp_processor::UdpProcessor;

pub enum Error {
    EnableOneTimeAuthFailed,
    NotOneTimeAuthSession,
    ConnectFailed(String),
    EncryptFailed,
    DecryptFailed,
    NoServerAvailable,
    InitEncryptorFailed(CryptoError),
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::EnableOneTimeAuthFailed => write!(f, "enable one time auth failed"),
            Error::NotOneTimeAuthSession => {
                write!(f, "current connection is not a one time auth session")
            }
            Error::ConnectFailed(ref e) => write!(f, "connect to server failed ({})", e),
            Error::EncryptFailed => write!(f, "encrypt data failed"),
            Error::DecryptFailed => write!(f, "decrypt data failed"),
            Error::NoServerAvailable => write!(f, "no ssserver available"),
            Error::InitEncryptorFailed(ref e) => write!(f, "init encryptor failed ({:?})", e),
        }
    }
}

impl From<CryptoError> for Error {
    fn from(e: CryptoError) -> Error {
        Error::InitEncryptorFailed(e)
    }
}

#[derive(Clone)]
pub enum Relay {
    Tcp(RcCell<TcpRelay>),
    Udp(RcCell<UdpRelay>),
}

impl Handler for Relay {
    type Message = ();
    type Timeout = Token;

    fn ready(&mut self, event_loop: &mut EventLoop<Relay>, token: Token, events: EventSet) {
        match self.clone() {
            Relay::Tcp(r) => {
                r.borrow_mut().ready(event_loop, token, events);
            }
            Relay::Udp(r) => {
                r.borrow_mut().ready(event_loop, token, events);
            }
        }
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Relay>, token: Token) {
        match self.clone() {
            Relay::Tcp(r) => {
                r.borrow_mut().timeout(event_loop, token);
            }
            Relay::Udp(r) => {
                r.borrow_mut().timeout(event_loop, token);
            }
        }
    }
}

pub trait MyHandler {
    fn ready(&mut self, event_loop: &mut EventLoop<Relay>, token: Token, events: EventSet);
    fn timeout(&mut self, event_loop: &mut EventLoop<Relay>, token: Token);
}

fn init_relay<T: MyHandler, P: Caller, F>(f: F) -> Result<T>
    where F: FnOnce(Token,
                    Token,
                    RcCell<DnsResolver>,
                    RcCell<ServerChooser>,
                    Holder<RcCell<P>>,
                    SocketAddr)
                    -> Result<T>
{
    let mut processors = Holder::new();
    let token = processors.alloc_token().ok_or(SocketError::AllocTokenFailed)?;
    let dns_token = processors.alloc_token().ok_or(SocketError::AllocTokenFailed)?;

    let mut dns_resolver = DnsResolver::new(dns_token, None, CONFIG.prefer_ipv6)?;
    let server_chooser = ServerChooser::new();

    let host = CONFIG.address().clone();
    let port = CONFIG.port();

    let HostIpPair(_host, ip) = dns_resolver.block_resolve(host)
        .and_then(|h| h.ok_or(From::from(DnsError::Timeout)))?;

    let socket_addr = pair2addr(&ip, port)?;

    f(token,
      dns_token,
      new_rc_cell(dns_resolver),
      new_rc_cell(server_chooser),
      processors,
      socket_addr)
}

mod tcp_relay;
mod udp_relay;
mod tcp_processor;
mod udp_processor;
