use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct EthernetDevice {
    pub incoming: VecDeque<Vec<u8>>,
    pub outgoing: VecDeque<Vec<u8>>,
}
pub(crate) struct Receive(Vec<u8>);
pub(crate) struct Transmit<'a>(&'a mut VecDeque<Vec<u8>>);
impl Device for EthernetDevice {
    type RxToken<'a> = Receive;
    type TxToken<'a> = Transmit<'a>;
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(64);
        caps
    }
    fn receive(&mut self, _: Instant) -> Option<(Receive, Transmit<'_>)> {
        self.incoming.pop_front().map(|frame| (Receive(frame), Transmit(&mut self.outgoing)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Transmit<'_>> {
        Some(Transmit(&mut self.outgoing))
    }
}
impl RxToken for Receive {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}
impl TxToken for Transmit<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        self.0.push_back(frame);
        result
    }
}
