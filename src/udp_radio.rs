#![macro_use]
use super::{
    debugln, prometheus_service as prometheus, prometheus_service::Stat, udp_runtime, INSTANT,
};
use heapless::consts::*;
use heapless::Vec as HVec;
use lorawan_device::{
    self as lorawan, radio, Device as LorawanDevice, Event as LorawanEvent,
    Response as LorawanResponse, Timings,
};
use semtech_udp::{push_data, push_data::RxPk, Down, Packet, StringOrNum};
use std::time::Duration;
use tokio::sync::{
    broadcast,
    mpsc::{self, Receiver, Sender},
};
use tokio::time::delay_for;

#[derive(Debug)]
#[allow(dead_code)]
pub enum Event {
    Rx(Box<semtech_udp::pull_resp::Packet>),
}

impl<'a> From<Box<semtech_udp::pull_resp::Packet>> for Event {
    fn from(rx: Box<semtech_udp::pull_resp::Packet>) -> Self {
        Event::Rx(rx)
    }
}

#[derive(Debug)]
// I need some intermediate event because of Lifetimes
// maybe there's a cleaner way of doing this
pub enum IntermediateEvent {
    Rx(Box<semtech_udp::pull_resp::Packet>, u64),
    NewSession,
    Timeout,
    SendPacket,
}

impl Settings {
    fn get_datr(&self) -> String {
        format!(
            "{}{}",
            match self.rfconfig.spreading_factor {
                radio::SpreadingFactor::_7 => "SF7",
                radio::SpreadingFactor::_8 => "SF8",
                radio::SpreadingFactor::_9 => "SF9",
                radio::SpreadingFactor::_10 => "SF10",
                radio::SpreadingFactor::_11 => "SF11",
                radio::SpreadingFactor::_12 => "SF12",
            },
            match self.rfconfig.bandwidth {
                radio::Bandwidth::_125KHZ => "BW125",
                radio::Bandwidth::_250KHZ => "BW250",
                radio::Bandwidth::_500KHZ => "BW500",
            }
        )
    }

    fn get_codr(&self) -> String {
        match self.rfconfig.coding_rate {
            radio::CodingRate::_4_5 => "4/5",
            radio::CodingRate::_4_6 => "4/6",
            radio::CodingRate::_4_7 => "4/7",
            radio::CodingRate::_4_8 => "4/8",
        }
        .to_string()
    }

    fn get_freq(&self) -> f64 {
        self.rfconfig.frequency as f64 / 1_000_000.0
    }
}

// Runtime translates UDP events into Device events
pub struct UdpRadioRuntime {
    receiver: broadcast::Receiver<udp_runtime::RxMessage>,
    lorawan_sender: Sender<IntermediateEvent>,
    time: Instant,
}

pub fn pretty_device(creds: &lorawan::Credentials) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend(creds.deveui());
    bytes.reverse();
    let hex = hex::encode(&bytes);
    hex.to_uppercase()[12..].to_string()
}

pub async fn run_loop(
    mut lorawan_receiver: Receiver<IntermediateEvent>,
    mut lorawan_sender: Sender<IntermediateEvent>,
    mut lorawan: LorawanDevice<UdpRadio>,
    mut prometheus: Option<Sender<prometheus::Message>>,
    transmit_delay: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    lorawan_sender
        .try_send(IntermediateEvent::NewSession)
        .unwrap();

    loop {
        let device_ref = pretty_device(lorawan.get_credentials());
        if let Some(event) = lorawan_receiver.recv().await {
            let mut time = None;
            let (new_state, response) = match event {
                IntermediateEvent::NewSession => {
                    // if jitter is enabled, we'll delay 0-127 ms
                    let delay = if lorawan.get_radio().jitter {
                        (super::get_random_u32() & 0x7F) as u64
                    } else {
                        0
                    };
                    delay_for(Duration::from_millis(delay as u64)).await;

                    debugln!("{}: Creating Session", device_ref);
                    let event = LorawanEvent::NewSession;
                    lorawan.handle_event(event)
                }
                IntermediateEvent::SendPacket => {
                    let data = [12, 3, 54, 54, 123, 23, 13, 14, 15, 16];
                    let mut ret = lorawan.send(&data, 2, true);
                    debugln!(
                        "{}: Sending DataUp, FcntUp = {}",
                        device_ref,
                        ret.0.get_fcnt_up().unwrap() - 1
                    );
                    ret
                }
                IntermediateEvent::Rx(packet, time_received) => {
                    time = Some(time_received);
                    lorawan.handle_event(LorawanEvent::RadioEvent(radio::Event::PhyEvent(
                        packet.into(),
                    )))
                }
                IntermediateEvent::Timeout => lorawan.handle_event(LorawanEvent::Timeout),
            };

            lorawan = new_state;

            match response {
                Ok(response) => match response {
                    LorawanResponse::TimeoutRequest(delay) => {
                        lorawan.get_radio().timer(delay).await;
                    }
                    LorawanResponse::NoJoinAccept => {
                        debugln!(
                                "{}: No JoinAccept Received",
                                device_ref,
                        );

                        // if the Join Request failed try again
                        lorawan_sender
                            .send(IntermediateEvent::NewSession)
                            .await
                            .unwrap();
                    }
                    LorawanResponse::NewSession => {
                        if let Some(t) = time {
                            debugln!(
                                "{}: JoinSuccess  [{} ms to spare] {:?}",
                                device_ref,
                                t,
                                lorawan.get_session_keys().unwrap()
                            );
                        }
                        let mut sender = lorawan_sender.clone();

                        tokio::spawn(async move {
                            delay_for(Duration::from_millis(transmit_delay as u64)).await;
                            sender.send(IntermediateEvent::SendPacket).await.unwrap();
                        });
                    }
                    LorawanResponse::Idle => (),
                    LorawanResponse::NoAck => {
                        debugln!("{}: NoAck", device_ref);
                        if let Some(ref mut sender) = prometheus {
                            sender
                                .send(prometheus::Message::Stat(device_ref, Stat::DownlinkTimeout))
                                .await?
                        }

                        let mut sender = lorawan_sender.clone();
                        tokio::spawn(async move {
                            delay_for(Duration::from_millis(transmit_delay as u64)).await;
                            sender.send(IntermediateEvent::SendPacket).await.unwrap();
                        });
                    }
                    LorawanResponse::ReadyToSend => {
                        debugln!(
                            "{}: No downlink received but none expected - ready to send again",
                            device_ref
                        );
                        let mut sender = lorawan_sender.clone();
                        tokio::spawn(async move {
                            delay_for(Duration::from_millis(transmit_delay as u64)).await;
                            sender.send(IntermediateEvent::SendPacket).await.unwrap();
                        });
                    }
                    LorawanResponse::DataDown(fcnt_down) => {
                        if let Some(t) = time {
                            debugln!(
                                "{}: DataDown [{} ms to spare], FcntDown = {} ",
                                device_ref,
                                t,
                                fcnt_down
                            );
                            if let Some(ref mut sender) = prometheus {
                                sender
                                    .send(prometheus::Message::Stat(
                                        device_ref,
                                        Stat::DownlinkResponse(t),
                                    ))
                                    .await?
                            }
                        }

                        // if jitter is enabled, we'll delay 0-127 ms
                        let delay = transmit_delay
                            + if lorawan.get_radio().jitter {
                                (super::get_random_u32() & 0x7F) as u64
                            } else {
                                0
                            };

                        let mut sender = lorawan_sender.clone();
                        tokio::spawn(async move {
                            delay_for(Duration::from_millis(delay as u64)).await;
                            sender.send(IntermediateEvent::SendPacket).await.unwrap();
                        });
                    }
                    LorawanResponse::Rxing => {
                        debugln!("{}: Receiving", device_ref);
                    }
                    _ => (),
                },
                Err(err) => match err {
                    lorawan::Error::Radio(_) => (),
                    lorawan::Error::Session(e) => {
                        use lorawan::session::Error;
                        match e {
                            Error::RadioEventWhileIdle
                            | Error::RadioEventWhileWaitingForRxWindow => (),
                            _ => panic!("LoRaWAN Error Session {:?}\r\n", e),
                        }
                    }
                    lorawan::Error::NoSession(e) => {
                        use lorawan::no_session::Error;
                        match e {
                            Error::RadioEventWhileIdle
                            | Error::RadioEventWhileWaitingForJoinWindow => (),
                            _ => panic!("LoRaWAN Error NoSession {:?}\r\n", e),
                        }
                    }
                },
            }
        }
    }
}

impl UdpRadioRuntime {
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // receive Semtech UDP packets from UDP Runtime
            let event = self.receiver.recv().await?;

            if let Packet::Down(Down::PullResp(pull_resp)) = event {
                let mut sender = self.lorawan_sender.clone();
                match &pull_resp.data.txpk.tmst {
                    StringOrNum::N(n) => {
                        let scheduled_time = n / 1000;
                        let time = self.time.elapsed().as_millis() as u64;
                        if scheduled_time > time {
                            // make units the same
                            let delay = scheduled_time - time as u64;
                            let event = IntermediateEvent::Rx(pull_resp.clone(), delay);
                            // dispatch the receive event only once its been received
                            tokio::spawn(async move {
                                delay_for(Duration::from_millis(delay + 50)).await;
                                sender.send(event).await.unwrap();
                            });
                        } else {
                            let time_since_scheduled_time = time - scheduled_time;
                            debugln!(
                                "Warning! UDP packet received after tx time by {} ms",
                                time_since_scheduled_time
                            );
                        }
                    }
                    StringOrNum::S(_) => {
                        debugln!("\tWarning! UDP packet sent with \"immediate\"");
                    }
                }
            }
        }
    }
}

use std::time::Instant;

#[derive(Default)]
struct Settings {
    rfconfig: radio::RfConfig,
}

impl From<radio::TxConfig> for Settings {
    fn from(txconfig: radio::TxConfig) -> Settings {
        Settings {
            rfconfig: txconfig.rf,
        }
    }
}

pub struct UdpRadio {
    sender: Sender<udp_runtime::TxMessage>,
    lorawan_sender: Sender<IntermediateEvent>,
    rx_buffer: HVec<u8, U256>,
    settings: Settings,
    time: Instant,
    window_start: u32,
    jitter: bool,
}

impl UdpRadio {
    pub fn new(
        sender: Sender<udp_runtime::TxMessage>,
        receiver: broadcast::Receiver<udp_runtime::RxMessage>,
        time: Instant,
    ) -> (
        Receiver<IntermediateEvent>,
        UdpRadioRuntime,
        Sender<IntermediateEvent>,
        UdpRadio,
    ) {
        let (lorawan_sender, lorawan_receiver) = mpsc::channel(100);
        let lorawan_sender_clone = lorawan_sender.clone();
        let lorawan_sender_another_clone = lorawan_sender.clone();
        (
            lorawan_receiver,
            UdpRadioRuntime {
                receiver,
                lorawan_sender,
                time,
            },
            lorawan_sender_another_clone,
            UdpRadio {
                sender,
                lorawan_sender: lorawan_sender_clone,
                rx_buffer: HVec::new(),
                settings: Settings {
                    rfconfig: radio::RfConfig::default(),
                },
                time,
                window_start: 0,
                jitter: true,
            },
        )
    }
    pub fn disable_jitter(&mut self) {
        self.jitter = false;
    }

    pub async fn timer(&mut self, future_time: u32) {
        let mut sender = self.lorawan_sender.clone();
        let delay = future_time - self.time.elapsed().as_millis() as u32;
        tokio::spawn(async move {
            delay_for(Duration::from_millis(delay as u64)).await;
            sender.send(IntermediateEvent::Timeout).await.unwrap();
        });
        self.window_start = delay;
    }
}

pub enum Error {}
pub enum Response {}

impl radio::PhyRxTx for UdpRadio {
    type PhyError = Error;
    type PhyResponse = Response;
    type PhyEvent = Event;
    fn get_mut_radio(&mut self) -> &mut Self {
        self
    }

    fn get_received_packet(&mut self) -> &mut HVec<u8, U256> {
        &mut self.rx_buffer
    }

    fn handle_event(
        &mut self,
        event: radio::Event<UdpRadio>,
    ) -> Result<radio::Response<UdpRadio>, radio::Error<UdpRadio>> {
        match event {
            radio::Event::TxRequest(tx_config, buffer) => {
                let size = buffer.len() as u64;
                let data = base64::encode(buffer);
                let tmst = self.time.elapsed().as_micros() as u64;

                let settings = Settings::from(tx_config);

                let rxpk = RxPk {
                    chan: 0,
                    codr: settings.get_codr(),
                    data,
                    datr: settings.get_datr(),
                    freq: settings.get_freq(),
                    lsnr: 5.5,
                    modu: "LORA".to_string(),
                    rfch: 0,
                    rssi: -112,
                    size,
                    stat: 1,
                    tmst,
                };
                let packet = push_data::Packet::from_rxpk(rxpk);

                if let Err(e) = self.sender.try_send(packet.into()) {
                    panic!("UdpTx Queue Overflow! {}", e)
                }

                Ok(radio::Response::TxDone(
                    self.time.elapsed().as_millis() as u32
                ))
            }
            radio::Event::RxRequest(config) => {
                self.settings.rfconfig = config;
                Ok(radio::Response::Idle)
            }
            radio::Event::CancelRx => Ok(radio::Response::Idle),
            radio::Event::PhyEvent(udp_event) => match udp_event {
                Event::Rx(pkt) => match base64::decode(&pkt.data.txpk.data) {
                    Ok(data) => {
                        self.rx_buffer.clear();
                        for el in data {
                            if let Err(e) = self.rx_buffer.push(el) {
                                panic!("Error pushing data into rx_buffer {}", e);
                            }
                        }
                        Ok(radio::Response::RxDone(radio::RxQuality::new(-115, 4)))
                    }
                    Err(e) => panic!("Semtech UDP Packet Decoding Error {}", e),
                },
            },
        }
    }
}

impl Timings for UdpRadio {
    fn get_rx_window_offset_ms(&mut self) -> i32 {
        20
    }
    fn get_rx_window_duration_ms(&mut self) -> u32 {
        100
    }
}
