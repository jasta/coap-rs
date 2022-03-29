use coap_lite::{CoapRequest, MessageType, ObserveOption, Packet, RequestType as Method, ResponseType as Status};
use log::{debug, warn};
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    net::SocketAddr,
    time::Duration,
};
use anyhow::anyhow;
use coap_lite::error::HandlingError;
use futures::stream::{Fuse, SelectNextSome};
use futures::StreamExt;
use tokio::time::interval;
use tokio_stream::wrappers::IntervalStream;
use crate::server::MessageReceiver;
use crate::synthetic_request::{SyntheticMessageReceiver, SyntheticMessageSender};

use super::server::MessageSender;

const DEFAULT_UNACKNOWLEDGE_MESSAGE_TRY_TIMES: usize = 10;

pub struct Observer {
    registers: HashMap<String, RegisterItem>,
    resources: HashMap<String, ResourceItem>,
    register_resources: HashMap<String, RegisterResourceItem>,
    unacknowledge_messages: HashMap<u16, UnacknowledgeMessageItem>,
    socket_tx: MessageSender,
    synthetic_request_tx: SyntheticMessageSender,
    synthetic_reply_rx: Fuse<SyntheticMessageReceiver>,
    current_message_id: u16,
    timer: Fuse<IntervalStream>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ObserveError {
    #[error("no observes for subject {0}")]
    NoObservers(String),
}

#[derive(Debug)]
struct RegisterItem {
    register_resources: HashSet<String>,
}

#[derive(Debug, Default)]
struct ResourceItem {
    register_resources: HashSet<String>,
    sequence: u32,
}

#[derive(Debug)]
struct RegisterResourceItem {
    register: String,
    resource: String,
    original_request: CoapRequest<SocketAddr>,
    unacknowledge_message: Option<u16>,
}

#[derive(Debug)]
struct UnacknowledgeMessageItem {
    register_resource: String,
    try_times: usize,
}

impl Observer {
    /// Creates an observer with channel to send message.
    pub fn new(socket_tx: MessageSender, synthetic_request_tx: SyntheticMessageSender, synthetic_reply_rx: SyntheticMessageReceiver) -> Observer {
        Observer {
            registers: HashMap::new(),
            resources: HashMap::new(),
            register_resources: HashMap::new(),
            unacknowledge_messages: HashMap::new(),
            socket_tx,
            synthetic_request_tx,
            synthetic_reply_rx: synthetic_reply_rx.fuse(),
            current_message_id: 0,
            timer: IntervalStream::new(interval(Duration::from_secs(1))).fuse(),
        }
    }

    /// filter the requests belong to the observer.
    pub async fn intercept_request(
        &mut self,
        request: &CoapRequest<SocketAddr>
    ) -> Result<bool, HandlingError> {
        if request.message.header.get_type() == MessageType::Acknowledgement {
            self.remove_unacknowledge_message(
                &request.message.header.message_id,
                &request.message.get_token(),
            )
        } else {
            Ok(false)
        }
    }

    /// intercept the request and response after the request handler has had a chance to
    /// generate the reply.
    pub async fn intercept_response(
        &mut self,
        request: &CoapRequest<SocketAddr>
    ) -> Result<bool, HandlingError> {
        if request.get_method() != &Method::Get &&
            request.get_method() != &Method::Fetch {
            return Ok(false);
        }
        if let Some(ref response) = request.response {
            if response.get_status() != &Status::Content &&
                response.get_status() != &Status::Valid {
                return Ok(false)
            }
        } else {
            return Ok(false)
        }

        if let Some(observe_flag) = request.get_observe_flag() {
            return match observe_flag {
                Ok(option) => {
                    match option {
                        ObserveOption::Register => self.register(request),
                        ObserveOption::Deregister => self.deregister(request),
                    }
                    Ok(true)
                },
                Err(err) => Err(HandlingError::bad_request(err.to_string())),
            }
        }

        Ok(false)
    }

    pub fn select_synthetic_reply(&mut self) -> SelectNextSome<Fuse<SyntheticMessageReceiver>> {
        self.synthetic_reply_rx.select_next_some()
    }

    pub fn select_timer(&mut self) -> SelectNextSome<Fuse<IntervalStream>> {
        self.timer.select_next_some()
    }

    /// trigger send the unacknowledge messages.
    pub async fn timer_handler(&mut self) {
        let register_resource_keys: Vec<String>;
        {
            register_resource_keys = self
                .unacknowledge_messages
                .iter()
                .map(|(_, msg)| msg.register_resource.clone())
                .collect();
        }

        for register_resource_key in register_resource_keys {
            if self.try_unacknowledge_message(&register_resource_key) {
                self.send_synthetic_request(&register_resource_key);
            }
        }
    }

    pub fn signal_resource_changed(&mut self, path: &str) -> Result<(), ObserveError> {
        let resource = self.resources.get(path)
            .ok_or_else(|| ObserveError::NoObservers(path.to_owned()))?;

        for register_resource_key in &resource.register_resources {
            self.send_synthetic_request(register_resource_key);
        }

        Ok(())
    }

    /// Issue a copy of the original request that will be handled internally and the reply sent
    /// back to us to be fixed up with an Observe option value then dispatched as a notification.
    // TODO: Would be nice to cache the requests and response pairs so that we don't duplicate
    // work in a 1-to-many observation situation...
    fn send_synthetic_request(&self, register_resource_key: &str) {
        let register_item = self.register_resources.get(register_resource_key).unwrap();
        let synthetic_request = register_item.original_request.clone();

        // Re-issue the original request and get the reply on `synthetic_reply_rx`.  Once we get it,
        // we'll issue the real socket tx for the notification.
        self.synthetic_request_tx.send((synthetic_request, register_resource_key.to_string()))
            .expect("tx channel closed unexpectedly");
    }

    pub async fn handle_synthetic_reply(&mut self, request: CoapRequest<SocketAddr>, register_resource_key: String) {
        if let Err(e) = self.handle_synthetic_reply_internal(request, register_resource_key).await {
            warn!("Error handling synthetic reply: {e:?}");
        }
    }

    async fn handle_synthetic_reply_internal(&mut self, mut request: CoapRequest<SocketAddr>, register_resource_key: String) -> anyhow::Result<()> {
        let register_item = self.register_resources.get(&register_resource_key)
            .ok_or_else(|| anyhow!("Registration canceled before synthetic reply available"))?;
        let resource = self.resources.get_mut(&register_item.resource)
            .ok_or_else(|| anyhow!("Internal error, resource removed???"))?;

        // TODO: We should technically be able to support non-confirmable notifications but
        // we don't really have an API for that yet.
        let packet = &mut request.message;
        packet.header.set_type(MessageType::Confirmable);

        // Update with the new message_id, note though we do not need to copy the token
        // as that's handled for us by storing the original request.
        self.current_message_id += 1;
        packet.header.message_id = self.current_message_id;

        resource.sequence += 1;
        packet.set_observe_value(resource.sequence);

        let address = register_item.original_request.source.unwrap();
        self.send_message(&address, packet).await;

        self.record_unacknowledge_message(&register_resource_key);

        Ok(())
    }

    fn register(&mut self, request: &CoapRequest<SocketAddr>) {
        let register_address = request.source.unwrap();
        let resource_path = request.get_path();

        debug!("register {} {}", register_address, resource_path);

        self.resources.entry(resource_path).or_default();

        // We need to make it again from the request packet so we don't end up cloning and re-using
        // the original reply in subsequent synthetic requests.
        let original_request = CoapRequest::from_packet(request.message.clone(), request.source.unwrap());
        self.record_register_resource(original_request);
    }

    fn deregister(&mut self, request: &CoapRequest<SocketAddr>) {
        let register_address = request.source.unwrap();
        let resource_path = request.get_path();

        debug!("deregister {} {}", register_address, resource_path);

        self.remove_register_resource(
            &register_address,
            &resource_path,
            &request.message.get_token(),
        );
    }

    fn record_register_resource(&mut self, request: CoapRequest<SocketAddr>) {
        let address: SocketAddr = request.source.unwrap();
        let path = request.get_path();
        let resource = self.resources.get_mut(&path).unwrap();
        let register_key = Self::format_register(&address);
        let register_resource_key = Self::format_register_resource(&address, &path);

        self.register_resources
            .entry(register_resource_key.clone())
            .or_insert(RegisterResourceItem {
                register: register_key.clone(),
                resource: path.clone(),
                original_request: request,
                unacknowledge_message: None,
            });
        resource
            .register_resources
            .replace(register_resource_key.clone());
        match self.registers.entry(register_key) {
            Entry::Occupied(register) => {
                register
                    .into_mut()
                    .register_resources
                    .replace(register_resource_key);
            }
            Entry::Vacant(v) => {
                let mut register = RegisterItem {
                    register_resources: HashSet::new(),
                };
                register.register_resources.insert(register_resource_key);

                v.insert(register);
            }
        };
    }

    fn remove_register_resource(
        &mut self,
        address: &SocketAddr,
        path: &String,
        token: &[u8],
    ) -> bool {
        let register_resource_key = Self::format_register_resource(&address, path);

        if let Some(register_resource) = self.register_resources.get(&register_resource_key) {
            if register_resource.token != *token {
                return false;
            }

            if let Some(unacknowledge_message) = register_resource.unacknowledge_message {
                self.unacknowledge_messages
                    .remove(&unacknowledge_message)
                    .unwrap();
            }

            assert_eq!(
                self.resources
                    .get_mut(path)
                    .unwrap()
                    .register_resources
                    .remove(&register_resource_key),
                true
            );

            let remove_register;
            {
                let register = self.registers.get_mut(&register_resource.register).unwrap();
                assert_eq!(
                    register.register_resources.remove(&register_resource_key),
                    true
                );
                remove_register = register.register_resources.len() == 0;
            }

            if remove_register {
                self.registers.remove(&register_resource.register);
            }
        }

        self.register_resources.remove(&register_resource_key);
        return true;
    }

    fn record_resource(&mut self, path: &String) -> &ResourceItem {
        match self.resources.entry(path.clone()) {
            Entry::Occupied(resource) => {
                let mut r = resource.into_mut();
                r.sequence += 1;
                return r;
            }
            Entry::Vacant(v) => {
                return v.insert(ResourceItem {
                    register_resources: HashSet::new(),
                    sequence: 0,
                });
            }
        }
    }

    fn record_unacknowledge_message(&mut self, register_resource_key: &String) {
        let message_id = self.current_message_id;

        let register_resource = self
            .register_resources
            .get_mut(register_resource_key)
            .unwrap();
        if let Some(old_message_id) = register_resource.unacknowledge_message {
            self.unacknowledge_messages.remove(&old_message_id);
        }

        register_resource.unacknowledge_message = Some(message_id);
        self.unacknowledge_messages.insert(
            message_id,
            UnacknowledgeMessageItem {
                register_resource: register_resource_key.clone(),
                try_times: 1,
            },
        );
    }

    fn try_unacknowledge_message(&mut self, register_resource_key: &String) -> bool {
        let register_resource = self
            .register_resources
            .get_mut(register_resource_key)
            .unwrap();
        let ref message_id = register_resource.unacknowledge_message.unwrap();

        let try_again;
        {
            let unacknowledge_message = self.unacknowledge_messages.get_mut(message_id).unwrap();
            if unacknowledge_message.try_times > DEFAULT_UNACKNOWLEDGE_MESSAGE_TRY_TIMES {
                try_again = false;
            } else {
                unacknowledge_message.try_times += 1;
                try_again = true;
            }
        }

        if !try_again {
            warn!(
                "unacknowledge_message try times exceeded  {}",
                register_resource_key
            );

            register_resource.unacknowledge_message = None;
            self.unacknowledge_messages.remove(message_id);
        }

        return try_again;
    }

    fn remove_unacknowledge_message(
        &mut self,
        message_id: &u16,
        token: &[u8]
    ) -> Result<bool, HandlingError> {
        if let Some(message) = self.unacknowledge_messages.get_mut(message_id) {
            let register_resource = self
                .register_resources
                .get_mut(&message.register_resource)
                .unwrap();
            if register_resource.token != *token {
                return Err(HandlingError::internal("message id <-> token mismatch, cannot process ack!"));
            }

            register_resource.unacknowledge_message = None;
        }

        Ok(self.unacknowledge_messages.remove(message_id).is_some())
    }

    async fn send_message(&mut self, address: &SocketAddr, message: &Packet) {
        debug!("send_message {:?} {:?}", address, message);
        self.socket_tx.send((message.clone(), *address)).unwrap();
    }

    fn format_register(address: &SocketAddr) -> String {
        format!("{}", address)
    }

    fn format_register_resource(address: &SocketAddr, path: &String) -> String {
        format!("{}${}", address, path)
    }
}

#[cfg(test)]
mod test {
    use super::super::*;
    use super::*;
    use coap_lite::CoapResponse;
    use std::{io::ErrorKind, sync::mpsc, time::Duration};

    async fn request_handler(req: CoapRequest<SocketAddr>) -> Option<CoapResponse> {
        match req.get_method() {
            &coap_lite::RequestType::Get => {
                let observe_option = req.get_observe_flag().unwrap().unwrap();
                assert_eq!(observe_option, ObserveOption::Deregister);
            }
            &coap_lite::RequestType::Put => {}
            _ => panic!("unexpected request"),
        }

        match req.response {
            Some(mut response) => {
                response.message.payload = b"OK".to_vec();
                Some(response)
            }
            _ => None,
        }
    }

    #[test]
    fn test_observe() {
        let path = "/test";
        let payload1 = b"data1".to_vec();
        let payload2 = b"data2".to_vec();
        let (tx, rx) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let mut step = 1;

        let server_port = server::test::spawn_server("127.0.0.1:0", request_handler)
            .recv()
            .unwrap();

        let server_address = &format!("127.0.0.1:{}", server_port);

        let mut client = CoAPClient::new(server_address).unwrap();

        tx.send(step).unwrap();
        let mut request = CoapRequest::new();
        request.set_method(coap_lite::RequestType::Put);
        request.set_path(path);
        request.message.payload = payload1.clone();
        client.send(&request).unwrap();
        client.receive().unwrap();

        let payload1_clone = payload1.clone();
        let payload2_clone = payload2.clone();

        let mut receive_step = 1;
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

        let client2 = CoAPClient::new(server_address).unwrap();
        client2.send(&request).unwrap();
        client2.receive().unwrap();
        assert_eq!(rx2.recv_timeout(Duration::new(5, 0)).unwrap(), ());
    }

    #[test]
    fn test_unobserve() {
        let path = "/test";
        let payload1 = b"data1".to_vec();
        let payload2 = b"data2".to_vec();

        let server_port = server::test::spawn_server("127.0.0.1:0", request_handler)
            .recv()
            .unwrap();

        let server_address = &format!("127.0.0.1:{}", server_port);

        let mut client = CoAPClient::new(server_address).unwrap();

        let mut request = CoapRequest::new();
        request.set_method(coap_lite::RequestType::Put);
        request.set_path(path);
        request.message.payload = payload1.clone();
        client.send(&request).unwrap();
        client.receive().unwrap();

        let payload1_clone = payload1.clone();
        client
            .observe(path, move |msg| {
                assert_eq!(msg.payload, payload1_clone);
            })
            .unwrap();

        client.unobserve();

        request.message.payload = payload2.clone();

        let client3 = CoAPClient::new(server_address).unwrap();
        client3.send(&request).unwrap();
        client3.receive().unwrap();
    }

    #[test]
    fn test_observe_without_resource() {
        let path = "/test";

        let server_port = server::test::spawn_server("127.0.0.1:0", request_handler)
            .recv()
            .unwrap();

        let mut client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let error = client.observe(path, |_msg| {}).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }
}
