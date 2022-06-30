use crate::ioctl::IoctlCmd;
use crate::prelude::*;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Timeout {
    sender: Option<Duration>,
    receiver: Option<Duration>,
}

impl Timeout {
    pub fn new() -> Self {
        Self {
            sender: None,
            receiver: None,
        }
    }

    pub fn sender_timeout(&self) -> Option<Duration> {
        self.sender
    }

    pub fn receiver_timeout(&self) -> Option<Duration> {
        self.receiver
    }

    pub fn set_sender(&mut self, timeout: Duration) {
        self.sender = Some(timeout);
    }

    pub fn set_receiver(&mut self, timeout: Duration) {
        self.receiver = Some(timeout);
    }
}

#[derive(Debug)]
pub struct SetSendTimeoutCmd(Duration);

impl IoctlCmd for SetSendTimeoutCmd {}

impl SetSendTimeoutCmd {
    pub fn new(timeout: Duration) -> Self {
        Self(timeout)
    }

    pub fn timeout(&self) -> &Duration {
        &self.0
    }
}

#[derive(Debug)]
pub struct SetRecvTimeoutCmd(Duration);

impl IoctlCmd for SetRecvTimeoutCmd {}

impl SetRecvTimeoutCmd {
    pub fn new(timeout: Duration) -> Self {
        Self(timeout)
    }

    pub fn timeout(&self) -> &Duration {
        &self.0
    }
}
