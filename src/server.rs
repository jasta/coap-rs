use coap_lite::{CoapRequest, CoapResponse, Packet};
use futures::{select, SinkExt, Stream, stream::FusedStream, StreamExt, task::Poll};
use log::{debug, error};
use std::{
    self,
    net::{self, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    pin::Pin,
    task::Context,
};
use tokio::{
    io,
    net::UdpSocket,
    sync::mpsc::{self},
};
use futures::stream::Fuse;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::udp::UdpFramed;
use coap_lite::error::HandlingError;
use crate::request_handler::{RequestHandler, ResponseBehaviour};
use crate::synthetic_request::{SyntheticMessageReceiver, SyntheticMessageSender, SyntheticRequest, SyntheticResponse};

use super::message::Codec;
use super::observer::Observer;

pub type MessageSender = mpsc::UnboundedSender<(Packet, SocketAddr)>;
pub(crate) type MessageReceiver = UnboundedReceiverStream<(Packet, SocketAddr)>;

#[derive(Debug)]
pub enum CoAPServerError {
    NetworkError,
    EventLoopError,
    AnotherHandlerIsRunning,
    EventSendError,
}

#[derive(Debug)]
pub struct QueuedMessage {
    pub address: SocketAddr,
    pub message: Packet,
}

pub enum Message {
    /// Packet to be sent to the remote peer.
    NeedSend(Packet, SocketAddr),

    /// Packet received from the remote peer.
    Received(Packet, SocketAddr),
}

pub struct Server<'a> {
    socket_tx: MessageSender,
    synthetic_request_rx: Fuse<SyntheticMessageReceiver>,
    synthetic_reply_tx: SyntheticMessageSender,
    server: CoAPServer,
    observer: Observer,
    handler: Option<Box<dyn RequestHandler + 'a>>,
}

impl<'a> Server<'a> {
    /// Creates a CoAP server listening on the given address.
    pub fn new<A: ToSocketAddrs>(addr: A) -> Result<Server<'a>, io::Error> {
        let (socket_tx, socket_rx) = mpsc::unbounded_channel();
        let (synthetic_request_tx, synthetic_request_rx) = mpsc::unbounded_channel();
        let (synthetic_reply_tx, synthetic_reply_rx) = mpsc::unbounded_channel();
        let server = CoAPServer::new(addr, socket_rx)?;
        let observer = Observer::new(
            socket_tx.clone(),
            synthetic_request_tx,
            UnboundedReceiverStream::new(synthetic_reply_rx));
        Ok(Server {
            socket_tx,
            synthetic_request_rx: UnboundedReceiverStream::new(synthetic_request_rx).fuse(),
            synthetic_reply_tx,
            server,
            observer,
            handler: None,
        })
    }

    /// run the server.
    pub async fn run(&mut self, handler: impl RequestHandler + 'a) -> Result<(), io::Error> {
        self.handler = Some(Box::new(handler));

        loop {
            select! {
                message = self.server.select_next_some() => {
                    match message {
                        Ok(Message::NeedSend(packet, addr)) => {
                            self.server.send((packet, addr)).await?;
                        }
                        Ok(Message::Received(packet, addr)) => {
                            self.handle_request(packet, addr).await?;
                        }
                        Err(e) => {
                            error!("select error: {:?}", e);
                        }
                    }
                }
                synthetic = self.synthetic_request_rx.select_next_some() => {
                    self.handle_synthetic_request(synthetic.0, synthetic.1, self.synthetic_reply_tx).await?
                }
                _ = self.observer.select_timer() => {
                    self.observer.timer_handler().await;
                }
                message = self.observer.select_synthetic_reply() => {
                    self.observer.handle_synthetic_reply(message.0, message.1).await;
                }
                complete => break,
            }
        }
        Ok(())
    }

    /// Return the local address that the server is listening on. This can be useful when starting
    /// a server on a random port as part of unit testing.
    pub fn socket_addr(&self) -> std::io::Result<SocketAddr> {
        self.server.socket_addr()
    }

    async fn handle_request(&mut self, packet: Packet, addr: SocketAddr) -> Result<(), io::Error> {
        let mut request = CoapRequest::from_packet(packet, addr);
        let behaviour = self.handle_request_internal(&mut request).await
            .unwrap_or_else(|err| {
                match request.apply_from_error(err) {
                    true => ResponseBehaviour::Normal,
                    false => ResponseBehaviour::Swallow,
                }
            });

        match behaviour {
            ResponseBehaviour::Normal => {
                if let Some(response) = request.response {
                    self.schedule_send_response(response, addr);
                }
            },
            ResponseBehaviour::Swallow => {
                debug!("Swallowing reply from {:?} for {}", addr, request.get_path());
            }
        }
        Ok(())
    }

    async fn handle_request_internal(&mut self, request: &mut CoapRequest<SocketAddr>) -> Result<ResponseBehaviour, HandlingError> {
        if self.observer.intercept_request(&request).await? {
            return Ok(ResponseBehaviour::Normal);
        }

        if let Some(ref mut handler) = self.handler {
            let behaviour = handler.handle_request(request).await;
            if behaviour == ResponseBehaviour::Normal {
                self.observer.intercept_response(&request).await?;
            }
            Ok(behaviour)
        } else {
            Ok(ResponseBehaviour::Swallow)
        }
    }

    async fn handle_synthetic_request(&mut self, request: CoapRequest<SocketAddr>, opaque_extra: String) {
        if let Some(ref mut handler) = self.handler {
            handler.handle_request(&mut request);
        }
    }

    fn schedule_send_response(&self, response: CoapResponse, addr: SocketAddr) {
        self.socket_tx.send((response.message, addr))
            .expect("tx channel closed unexpectedly");
    }

    async fn resource_changed(&mut self, path: &str) {
        if let Err(err) = self.observer.signal_resource_changed(path) {
            debug!("signal_resource_changed: path={}, err={:?}", path, err);
        }
    }

    /// enable AllCoAP multicasts - adds the AllCoap addresses to the listener
    /// - IPv4 AllCoAP multicast address is '224.0.1.187'
    /// - IPv6 AllCoAp multicast addresses are 'ff0?::fd'
    ///
    /// Parameter segment is used with IPv6 to determine the first octet.
    /// - It's value can be between 0x0 and 0xf.
    /// - To join multiple segments, you have to call enable_discovery for each of the segments.
    ///
    /// For further details see method join_multicast
    pub fn enable_all_coap(&mut self, segment: u8) {
        assert!(segment <= 0xf);
        let socket = self.server.socket.get_mut();
        let m = match socket.local_addr().unwrap() {
            SocketAddr::V4(_val) => IpAddr::V4(Ipv4Addr::new(224, 0, 1, 187)),
            SocketAddr::V6(_val) => IpAddr::V6(Ipv6Addr::new(
                0xff00 + segment as u16,
                0,
                0,
                0,
                0,
                0,
                0,
                0xfd,
            )),
        };
        self.join_multicast(m);
    }

    /// join multicast - adds the multicast addresses to the unicast listener
    /// - IPv4 multicast address range is '224.0.0.0/4'
    /// - IPv6 AllCoAp multicast addresses are 'ff00::/8'
    ///
    /// Parameter segment is used with IPv6 to determine the first octet.
    /// - It's value can be between 0x0 and 0xf.
    /// - To join multiple segments, you have to call enable_discovery for each of the segments.
    ///
    /// Some Multicast address scope
    /// IPv6        IPv4 equivalent[16]	        Scope	            Purpose
    /// ffx1::/16	127.0.0.0/8	                Interface-local	    Packets with this destination address may not be sent over any network link, but must remain within the current node; this is the multicast equivalent of the unicast loopback address.
    /// ffx2::/16	224.0.0.0/24	            Link-local	        Packets with this destination address may not be routed anywhere.
    /// ffx3::/16	239.255.0.0/16	            IPv4 local scope
    /// ffx4::/16	            	            Admin-local	        The smallest scope that must be administratively configured.
    /// ffx5::/16		                        Site-local	        Restricted to the local physical network.
    /// ffx8::/16	239.192.0.0/14	            Organization-local	Restricted to networks used by the organization administering the local network. (For example, these addresses might be used over VPNs; when packets for this group are routed over the public internet (where these addresses are not valid), they would have to be enccapsulated in some other protocol.)
    /// ffxe::/16	224.0.1.0-238.255.255.255	Global scope	    Eligible to be routed over the public internet.
    ///
    /// Notable addresses:
    /// ff02::1	    All nodes on the local network segment
    /// ff0x::c	    Simple Service Discovery Protocol
    /// ff0x::fb	Multicast DNS
    /// ff0x::fb	Multicast CoAP
    /// ff0x::114	Used for experiments
    pub fn join_multicast(&mut self, addr: IpAddr) {
        self.server.join_multicast(addr);
    }

    /// leave multicast - remove the multicast address from the listener
    pub fn leave_multicast(&mut self, addr: IpAddr) {
        self.server.leave_multicast(addr);
    }
}

pub struct CoAPServer {
    receiver: MessageReceiver,
    is_terminated: bool,
    socket: UdpFramed<Codec>,
    multicast_addresses: Vec<IpAddr>,
}

impl CoAPServer {
    /// Creates a CoAP server listening on the given address.
    pub fn new<A: ToSocketAddrs>(
        addr: A,
        rx: mpsc::UnboundedReceiver<(Packet, SocketAddr)>,
    ) -> Result<CoAPServer, io::Error> {
        let std_socket = net::UdpSocket::bind(addr).unwrap();
        std_socket.set_nonblocking(true)?;

        let socket = UdpSocket::from_std(std_socket)?;

        Ok(CoAPServer {
            receiver: UnboundedReceiverStream::new(rx),
            is_terminated: false,
            socket: UdpFramed::new(socket, Codec::new()),
            multicast_addresses: Vec::new(),
        })
    }

    /// Stop the server.
    pub fn stop(&mut self) {
        self.is_terminated = true;
    }

    /// send the packet to the specific address.
    pub async fn send(&mut self, frame: (Packet, SocketAddr)) -> Result<(), io::Error> {
        self.socket.send(frame).await
    }

    /// Return the local address that the server is listening on. This can be useful when starting
    /// a server on a random port as part of unit testing.
    pub fn socket_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.get_ref().local_addr()
    }

    /// join multicast - adds the multicast addresses to the listener
    pub fn join_multicast(&mut self, addr: IpAddr) {
        assert!(addr.is_multicast());
        let socket = self.socket.get_mut();
        // determine wether IPv4 or IPv6 and
        // join the appropriate multicast address
        match socket.local_addr().unwrap() {
            SocketAddr::V4(val) => {
                match addr {
                    IpAddr::V4(ipv4) => {
                        let i = val.ip().clone();
                        socket.join_multicast_v4(ipv4, i).unwrap();
                        self.multicast_addresses.push(addr);
                    }
                    IpAddr::V6(_ipv6) => { /* handle IPv6 */ }
                }
            }
            SocketAddr::V6(_val) => {
                match addr {
                    IpAddr::V4(_ipv4) => { /* handle IPv4 */ }
                    IpAddr::V6(ipv6) => {
                        socket.join_multicast_v6(&ipv6, 0).unwrap();
                        self.multicast_addresses.push(addr);
                        //socket.set_only_v6(true)?;
                    }
                }
            }
        }
    }

    /// leave multicast - remove the multicast address from the listener
    pub fn leave_multicast(&mut self, addr: IpAddr) {
        assert!(addr.is_multicast());
        let socket = self.socket.get_mut();
        // determine wether IPv4 or IPv6 and
        // leave the appropriate multicast address
        match socket.local_addr().unwrap() {
            SocketAddr::V4(val) => {
                match addr {
                    IpAddr::V4(ipv4) => {
                        let i = val.ip().clone();
                        socket.leave_multicast_v4(ipv4, i).unwrap();
                        let index = self
                            .multicast_addresses
                            .iter()
                            .position(|&item| item == addr)
                            .unwrap();
                        self.multicast_addresses.remove(index);
                    }
                    IpAddr::V6(_ipv6) => { /* handle IPv6 */ }
                }
            }
            SocketAddr::V6(_val) => {
                match addr {
                    IpAddr::V4(_ipv4) => { /* handle IPv4 */ }
                    IpAddr::V6(ipv6) => {
                        socket.leave_multicast_v6(&ipv6, 0).unwrap();
                        let index = self
                            .multicast_addresses
                            .iter()
                            .position(|&item| item == addr)
                            .unwrap();
                        self.multicast_addresses.remove(index);
                    }
                }
            }
        }
    }
}

impl Drop for CoAPServer {
    fn drop(&mut self) {
        // unregister still existing multicast addresses
        let socket = self.socket.get_mut();
        for addr in &self.multicast_addresses {
            match addr {
                IpAddr::V4(ipv4) => match socket.local_addr().unwrap() {
                    SocketAddr::V4(val) => {
                        socket.leave_multicast_v4(*ipv4, val.ip().clone()).unwrap();
                    }
                    _ => {
                        panic!("should not happen");
                    }
                },
                IpAddr::V6(ipv6) => {
                    socket.leave_multicast_v6(&ipv6, 0).unwrap();
                }
            }
        }
        // stop server
        self.stop();
    }
}

impl Stream for CoAPServer {
    type Item = Result<Message, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some((p, a))) = self.receiver.poll_next_unpin(cx) {
            return Poll::Ready(Some(Ok(Message::NeedSend(p, a))));
        }

        let result: Option<_> = futures::ready!(self.socket.poll_next_unpin(cx));

        Poll::Ready(match result {
            Some(Ok(message)) => {
                let (my_packet, addr) = message;
                Some(Ok(Message::Received(my_packet, addr)))
            }
            Some(Err(e)) => Some(Err(e)),
            None => None,
        })
    }
}

impl FusedStream for CoAPServer {
    fn is_terminated(&self) -> bool {
        self.is_terminated
    }
}

#[cfg(test)]
pub mod test {
    use super::super::*;
    use super::*;
    use coap_lite::CoapOption;
    use std::{sync::mpsc, time::Duration};
    use async_trait::async_trait;
    use crate::request_handler::ResponseBehaviour;

    pub fn spawn_server(
        ip: &'static str,
        request_handler: impl RequestHandler + 'static,
    ) -> mpsc::Receiver<u16> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new()
            .name(String::from("server"))
            .spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(async move {
                        let mut server = server::Server::new(ip).unwrap();

                        tx.send(server.socket_addr().unwrap().port()).unwrap();

                        server.run(request_handler).await.unwrap();
                    })
            })
            .unwrap();

        rx
    }

    #[derive(Default)]
    struct TestRequestHandler;
    #[async_trait]
    impl RequestHandler for TestRequestHandler {
        async fn handle_request(&mut self, request: &mut CoapRequest<SocketAddr>) -> ResponseBehaviour {
            let uri_path_list = request.message.get_option(CoapOption::UriPath).unwrap().clone();
            assert_eq!(uri_path_list.len(), 1);

            if let Some(response) = &mut request.response {
                response.message.payload = uri_path_list.front().unwrap().clone();
            }
            ResponseBehaviour::default()
        }
    }

    pub fn spawn_server_with_all_coap(
        ip: &'static str,
        request_handler: impl RequestHandler + 'static,
        segment: u8,
    ) -> mpsc::Receiver<u16> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new()
            .name(String::from("v4-server"))
            .spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(async move {
                        // multicast needs a server on a real interface
                        let mut server = server::Server::new((ip, 0)).unwrap();
                        server.enable_all_coap(segment);

                        tx.send(server.socket_addr().unwrap().port()).unwrap();

                        server.run(request_handler).await.unwrap();
                    })
            })
            .unwrap();

        rx
    }

    #[test]
    fn test_echo_server() {
        let server_port = spawn_server("127.0.0.1:0", TestRequestHandler::default()).recv().unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    #[ignore]
    fn test_echo_server_v6() {
        let server_port = spawn_server("::1:0", TestRequestHandler::default()).recv().unwrap();

        let client = CoAPClient::new(format!("::1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    fn test_echo_server_no_token() {
        let server_port = spawn_server("127.0.0.1:0", TestRequestHandler::default()).recv().unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut packet = CoapRequest::new();
        packet.message.header.set_version(1);
        packet
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        packet.message.header.set_code("0.01");
        packet.message.header.message_id = 1;
        packet
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&packet).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    #[ignore]
    fn test_echo_server_no_token_v6() {
        let server_port = spawn_server("::1:0", TestRequestHandler::default()).recv().unwrap();

        let client = CoAPClient::new(format!("::1:{}", server_port)).unwrap();
        let mut packet = CoapRequest::new();
        packet.message.header.set_version(1);
        packet
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        packet.message.header.set_code("0.01");
        packet.message.header.message_id = 1;
        packet
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&packet).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    fn test_update_resource() {
        let path = "/test";
        let payload1 = b"data1".to_vec();
        let payload2 = b"data2".to_vec();
        let (tx, rx) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let mut step = 1;

        let server_port = spawn_server("127.0.0.1:0", TestRequestHandler::default()).recv().unwrap();

        let mut client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();

        tx.send(step).unwrap();
        let mut request = CoapRequest::new();
        request.set_method(coap_lite::RequestType::Put);
        request.set_path(path);
        request.message.payload = payload1.clone();
        client.send(&request).unwrap();
        client.receive().unwrap();

        let mut receive_step = 1;
        let payload1_clone = payload1.clone();
        let payload2_clone = payload2.clone();
        client
            .observe(path, move |msg| {
                match rx.try_recv() {
                    Ok(n) => receive_step = n,
                    _ => (),
                }

                match receive_step {
                    1 => assert_eq!(msg.payload, payload1_clone),
                    2 => {
                        assert_eq!(msg.payload, payload2_clone);
                        tx2.send(()).unwrap();
                    }
                    _ => panic!("unexpected step"),
                }
            })
            .unwrap();

        step = 2;
        tx.send(step).unwrap();
        request.message.payload = payload2.clone();
        let client2 = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        client2.send(&request).unwrap();
        client2.receive().unwrap();
        assert_eq!(rx2.recv_timeout(Duration::new(5, 0)).unwrap(), ());
    }

    #[test]
    fn multicast_server_all_coap() {
        // segment not relevant with IPv4
        let segment = 0x0;
        let server_port = spawn_server_with_all_coap("0.0.0.0", TestRequestHandler::default(), segment)
            .recv()
            .unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());

        let client = CoAPClient::new(format!("224.0.1.187:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::NonConfirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 2;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send_all_coap(&request, segment).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    //This test right now does not work on windows
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn multicast_server_all_coap_v6() {
        // use segment 0x04 which should be the smallest administered scope
        let segment = 0x04;
        let server_port = spawn_server_with_all_coap("::0", TestRequestHandler::default(), segment)
            .recv()
            .unwrap();

        let client = CoAPClient::new(format!("::1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());

        // use 0xff02 to keep it within this network
        let client = CoAPClient::new(format!("ff0{}::fd:{}", segment, server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request
            .message
            .header
            .set_type(coap_lite::MessageType::NonConfirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 2;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request
            .message
            .add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send_all_coap(&request, segment).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    fn multicast_join_leave() {
        std::thread::Builder::new()
            .name(String::from("v4-server"))
            .spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(async move {
                        // multicast needs a server on a real interface
                        let mut server = server::Server::new(("0.0.0.0", 0)).unwrap();
                        server.join_multicast(IpAddr::V4(Ipv4Addr::new(224, 0, 1, 1)));
                        server.join_multicast(IpAddr::V4(Ipv4Addr::new(224, 1, 1, 1)));
                        server.leave_multicast(IpAddr::V4(Ipv4Addr::new(224, 0, 1, 1)));
                        server.leave_multicast(IpAddr::V4(Ipv4Addr::new(224, 1, 1, 1)));
                        server.run(TestRequestHandler::default()).await.unwrap();
                    })
            })
            .unwrap();

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    #[test]
    #[ignore]
    fn multicast_join_leave_v6() {
        std::thread::Builder::new()
            .name(String::from("v6-server"))
            .spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(async move {
                        // multicast needs a server on a real interface
                        let mut server = server::Server::new(("::0", 0)).unwrap();
                        server.join_multicast(IpAddr::V6(Ipv6Addr::new(
                            0xff02, 0, 0, 0, 0, 0, 1, 0x1,
                        )));
                        server.join_multicast(IpAddr::V6(Ipv6Addr::new(
                            0xff02, 0, 0, 0, 0, 1, 0, 0x2,
                        )));
                        server.leave_multicast(IpAddr::V6(Ipv6Addr::new(
                            0xff02, 0, 0, 0, 0, 0, 1, 0x1,
                        )));
                        server.join_multicast(IpAddr::V6(Ipv6Addr::new(
                            0xff02, 0, 0, 0, 0, 1, 0, 0x2,
                        )));
                        server.run(TestRequestHandler::default()).await.unwrap();
                    })
            })
            .unwrap();

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
