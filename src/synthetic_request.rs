use std::net::SocketAddr;

use coap_lite::{CoapRequest, Packet};
use coap_lite::error::HandlingError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub struct SyntheticChannelRequester {
    pub tx: UnboundedSender<SyntheticRequest>,
    pub rx: UnboundedReceiver<SyntheticResponse>,
}

pub struct SyntheticChannelResponder {
    pub tx: UnboundedSender<SyntheticResponse>,
    pub rx: UnboundedReceiver<SyntheticRequest>,
}

#[derive(Debug, Clone)]
pub struct SyntheticRequest {
    request: CoapRequest<SocketAddr>,
    opaque_extra: String,
}

#[derive(Debug, Clone)]
pub struct SyntheticResponse {
    result: Result<(Packet, SocketAddr), HandlingError>,
    opaque_extra: String,
}
