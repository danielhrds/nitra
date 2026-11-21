//! Local signaling: TCP on 127.0.0.1 between processes on the same PC.
//! The host listens; each viewer connects, sends its offer, gets an answer,
//! and keeps the connection until one side says bye or closes it.

use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{read_message, write_message};

/// How long a peer has to send its next message during setup.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct LocalListener {
    listener: TcpListener,
}

impl LocalListener {
    /// Listens on 127.0.0.1 only (port 0: any free port).
    pub fn bind(port: u16) -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind((Ipv4Addr::LOCALHOST, port))?,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Waits for the next viewer.
    pub fn accept(&self) -> io::Result<LocalChannel> {
        let (stream, _) = self.listener.accept()?;
        LocalChannel::new(stream)
    }
}

/// One signaling connection.
pub struct LocalChannel {
    stream: TcpStream,
}

impl LocalChannel {
    /// Connects to a host's listener (127.0.0.1 only).
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local signaling is 127.0.0.1 only",
            ));
        }
        Self::new(TcpStream::connect_timeout(&addr, SETUP_TIMEOUT)?)
    }

    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(SETUP_TIMEOUT))?;
        Ok(Self { stream })
    }

    pub fn send(&mut self, message: &impl Serialize) -> io::Result<()> {
        write_message(&mut self.stream, message)
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        read_message(&mut self.stream)
    }

    /// After setup: wait without a timeout (a session may last hours).
    pub fn wait_forever(&mut self) -> io::Result<()> {
        self.stream.set_read_timeout(None)
    }

    /// A handle to the same connection, e.g. to send `Bye` from another
    /// thread while this one waits in `recv`.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Ends the connection (unblocks a `recv` on a clone).
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    #[test]
    fn offer_answer_and_bye_over_loopback() {
        let listener = LocalListener::bind(0).unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let mut channel = listener.accept().unwrap();
            let Message::Offer { sdp, profile } = channel.recv().unwrap() else {
                panic!("expected an offer");
            };
            assert_eq!((sdp.as_str(), profile.as_str()), ("offer-sdp", "viewer"));
            channel
                .send(&Message::Answer {
                    sdp: "answer-sdp".into(),
                    profile: "host".into(),
                })
                .unwrap();
            channel.wait_forever().unwrap();
            channel.recv::<Message>().unwrap()
        });
        let mut viewer = LocalChannel::connect(addr).unwrap();
        viewer
            .send(&Message::Offer {
                sdp: "offer-sdp".into(),
                profile: "viewer".into(),
            })
            .unwrap();
        assert_eq!(
            viewer.recv::<Message>().unwrap(),
            Message::Answer {
                sdp: "answer-sdp".into(),
                profile: "host".into(),
            }
        );
        let bye = Message::Bye {
            reason: "closing".into(),
        };
        viewer.send(&bye).unwrap();
        assert_eq!(host.join().unwrap(), bye);
    }

    #[test]
    fn closing_the_connection_ends_a_waiting_recv() {
        let listener = LocalListener::bind(0).unwrap();
        let addr = listener.local_addr().unwrap();
        let viewer = LocalChannel::connect(addr).unwrap();
        let mut host = listener.accept().unwrap();
        host.wait_forever().unwrap();
        let waiting = std::thread::spawn(move || host.recv::<Message>().is_err());
        viewer.shutdown();
        assert!(waiting.join().unwrap());
    }

    #[test]
    fn only_loopback_addresses_are_accepted() {
        assert!(LocalChannel::connect("192.0.2.1:9".parse().unwrap()).is_err());
    }
}
