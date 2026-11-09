//! `orico-raid`: query JMicron (JMB39x/JMS56x) hardware RAID enclosures
//! (ORICO 9558RU3, 9528RU3, ...) from Linux without the vendor GUI.

pub mod api;
pub mod io;
#[doc(hidden)]
pub mod mock;
pub mod model;
pub mod proto;
pub mod render;
pub mod scan;
pub mod session;
