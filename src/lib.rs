extern crate core;

pub mod clock;
pub mod socket;
pub mod tlvforwarder;

use std::{
    future::Future,
    pin::{pin, Pin},
    str::FromStr,
    sync::{Arc, RwLock},
};

use clock::{LinuxClock, PortTimestampToTime};
use rand::{rngs::StdRng, SeedableRng};
use socket::{open_ipv4_event_socket, open_ipv4_general_socket, PtpTargetAddress};
use statime::{
    config::{ClockIdentity, InstanceConfig, PortConfig, SdoId, TimePropertiesDS},
    filters::{KalmanConfiguration, KalmanFilter},
    port::{
        is_message_buffer_compatible, InBmca, Port, PortAction, PortActionIterator,
        TimestampContext, MAX_DATA_LEN,
    },
    time::{Duration, Interval, Time},
    Clock, OverlayClock, PtpInstance, PtpInstanceState, SharedClock,
};
use timestamped_socket::{
    interface::InterfaceName,
    networkaddress::NetworkAddress,
    socket::{Open, RecvResult, Timestamp},
};
use tlvforwarder::TlvForwarder;
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::Sleep,
};

pin_project_lite::pin_project! {
    struct Timer {
        #[pin]
        timer: Sleep,
        running: bool,
    }
}

impl Timer {
    fn new() -> Self {
        Timer {
            timer: tokio::time::sleep(std::time::Duration::from_secs(0)),
            running: false,
        }
    }

    fn reset(self: Pin<&mut Self>, duration: std::time::Duration) {
        let this = self.project();
        this.timer.reset(tokio::time::Instant::now() + duration);
        *this.running = true;
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.project();
        if *this.running {
            let result = this.timer.poll(cx);
            if result != std::task::Poll::Pending {
                *this.running = false;
            }
            result
        } else {
            std::task::Poll::Pending
        }
    }
}

trait PortClock:
    Clock<Error = <LinuxClock as Clock>::Error> + PortTimestampToTime + Send + Sync
{
    fn clone_box(&self) -> Box<dyn PortClock>;
}

impl PortClock for LinuxClock {
    fn clone_box(&self) -> Box<dyn PortClock> {
        Box::new(self.clone())
    }
}
impl PortClock for SharedClock<OverlayClock<LinuxClock>> {
    fn clone_box(&self) -> Box<dyn PortClock> {
        Box::new(self.clone())
    }
}

type BoxedClock = Box<dyn PortClock>;
type SharedOverlayClock = SharedClock<OverlayClock<LinuxClock>>;

#[derive(Debug, Clone)]
enum SystemClock {
    Linux(LinuxClock),
    Overlay(SharedOverlayClock),
}

impl SystemClock {
    fn clone_boxed(&self) -> BoxedClock {
        match self {
            Self::Linux(clock) => Box::new(clock.clone()),
            Self::Overlay(clock) => Box::new(clock.clone()),
        }
    }
}

pub struct Socket<A: NetworkAddress + PtpTargetAddress + Send> {
    pub out_ch: tokio::sync::mpsc::Sender<(Vec<u8>, A)>,
    pub in_ch: tokio::sync::mpsc::Receiver<(Vec<u8>, Timestamp)>,
    pub timestamp_ch: tokio::sync::mpsc::Receiver<Timestamp>,
}

impl<A: NetworkAddress + PtpTargetAddress + Send> Socket<A> {
    async fn send_to(&mut self, buf: &[u8], addr: A) -> std::io::Result<Option<Timestamp>> {
        self.out_ch
            .send((buf.into(), addr))
            .await
            .expect("Closed channel");

        Ok(Some(
            self.timestamp_ch.recv().await.expect("Closed channel"),
        ))
    }

    async fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<RecvResult<A>> {
        let (msg, timestamp) = self.in_ch.recv().await.expect("Closed channel");
        buf.copy_from_slice(&msg);

        Ok(RecvResult {
            bytes_read: msg.len(),
            remote_addr: A::PRIMARY_GENERAL,
            timestamp: Some(timestamp),
        })
    }
}

pub async fn run_time_sync<A: NetworkAddress + PtpTargetAddress + Send + 'static>(
    announce_notifier: Arc<tokio::sync::Notify>,
    sync_notifier: Arc<tokio::sync::Notify>,
    general_socket: Socket<A>,
    event_socket: Socket<A>,
    // tunnel_name: &str,
    indentity: [u8; 6],
    priority: u8,
    is_master: bool,
    path_trace: bool,
    use_virtual_clock: bool,
    announce_interval: Interval,
    announce_receipt_timeout: u8, // wait until the announce message expires.
    sync_interval: Interval,
) -> ! {
    let time_properties_ds = TimePropertiesDS::new_arbitrary_time(
        false,
        false,
        statime::config::TimeSource::InternalOscillator,
    );

    // Leak to get a static reference, the ptp instance will be around for the rest
    // of the program anyway
    let instance = Box::leak(Box::new(PtpInstance::new(
        InstanceConfig {
            path_trace,
            clock_identity: statime::config::ClockIdentity::from_mac_address(indentity),
            priority_1: priority,
            priority_2: priority,
            domain_number: 0,
            sdo_id: SdoId::default(),
            slave_only: false,
        },
        time_properties_ds,
    )));

    let clock = if use_virtual_clock {
        SystemClock::Overlay(SharedClock::new(OverlayClock::new(LinuxClock::CLOCK_TAI)))
    } else {
        SystemClock::Linux(LinuxClock::CLOCK_TAI)
    };

    let rng = StdRng::from_entropy();
    let port = instance.add_port(
        PortConfig {
            announce_interval,
            announce_receipt_timeout,
            acceptable_master_list: None,
            sync_interval,
            master_only: is_master,
            delay_asymmetry: Duration::from_nanos(0), // The estimated asymmetry in the link connected to this [`Port`]
            delay_mechanism: statime::config::DelayMechanism::E2E {
                interval: Interval::from_log_2(0),
            },
        },
        KalmanConfiguration::default(),
        clock.clone_boxed(),
        rng,
    );

    let (bmca_notify_sender, bmca_notify_receiver) = tokio::sync::watch::channel(false);
    let (main_task_sender, port_task_receiver) = tokio::sync::mpsc::channel(1);
    let (port_task_sender, main_task_receiver) = tokio::sync::mpsc::channel(1);
    {
        // let interface_name =
        //     InterfaceName::from_str(tunnel_name).expect("Failed to parse interface name");
        // let event_socket = open_ipv4_event_socket(
        //     interface_name,
        //     // software clock supported only
        //     timestamped_socket::socket::InterfaceTimestampMode::SoftwareAll,
        //     None,
        // )
        // .expect("Failed to open event socket");
        // let general_socket =
        //     open_ipv4_general_socket(interface_name).expect("Failed to open general socket");

        tokio::spawn(port_task(
            announce_notifier,
            sync_notifier,
            port_task_receiver,
            port_task_sender,
            event_socket,
            general_socket,
            bmca_notify_receiver.clone(),
            TlvForwarder::new(),
            clock.clone_boxed(),
        ));
    }

    main_task_sender
        .send(port)
        .await
        .expect("Failed to send port");

    run(
        instance,
        bmca_notify_sender,
        main_task_receiver,
        main_task_sender,
    )
    .await
}

async fn run(
    instance: &'static PtpInstance<KalmanFilter, RwLock<PtpInstanceState>>,
    bmca_notify_sender: tokio::sync::watch::Sender<bool>,
    mut main_task_receiver: Receiver<BmcaPort>,
    main_task_sender: Sender<BmcaPort>,
) -> ! {
    // run bmca over all of the ports at the same time. The ports don't perform
    // their normal actions at this time: bmca is stop-the-world!
    let mut bmca_timer = pin!(Timer::new());

    loop {
        // reset bmca timer
        bmca_timer.as_mut().reset(instance.bmca_interval());

        // wait until the next BMCA
        bmca_timer.as_mut().await;

        // notify all the ports that they need to stop what they're doing
        bmca_notify_sender
            .send(true)
            .expect("Bmca notification failed");

        let mut bmca_port = main_task_receiver
            .recv()
            .await
            .expect("Failed to receive bcma port");

        // have all ports so deassert stop
        bmca_notify_sender
            .send(false)
            .expect("Bmca notification failed");

        instance.bmca(&mut [&mut bmca_port]);

        main_task_sender
            .send(bmca_port)
            .await
            .expect("Failed to send bmca_port");
    }
}
// the Port task
//
// This task waits for a new port (in the bmca state) to arrive on its Receiver.
// It will then move the port into the running state, and process actions. When
// the task is notified of a BMCA, it will stop running, move the port into the
// bmca state, and send it on its Sender
async fn port_task<A: NetworkAddress + PtpTargetAddress + Send>(
    announce_notifier: Arc<tokio::sync::Notify>,
    sync_notifier: Arc<tokio::sync::Notify>,
    mut port_task_receiver: Receiver<BmcaPort>,
    port_task_sender: Sender<BmcaPort>,
    mut event_socket: Socket<A>,
    mut general_socket: Socket<A>,
    mut bmca_notify: tokio::sync::watch::Receiver<bool>,
    mut tlv_forwarder: TlvForwarder,
    clock: BoxedClock,
) {
    let mut timers = Timers {
        port_announce_timeout_timer: pin!(Timer::new()),
        delay_request_timer: pin!(Timer::new()),
        filter_update_timer: pin!(Timer::new()),
    };

    loop {
        let port_in_bmca = port_task_receiver.recv().await.unwrap();

        // handle post-bmca actions
        let (mut port, actions) = port_in_bmca.end_bmca();

        let mut pending_timestamp = handle_actions(
            actions,
            &mut event_socket,
            &mut general_socket,
            &mut timers,
            &tlv_forwarder,
            &clock,
        )
        .await;

        while let Some((context, timestamp)) = pending_timestamp {
            pending_timestamp = handle_actions(
                port.handle_send_timestamp(context, timestamp),
                &mut event_socket,
                &mut general_socket,
                &mut timers,
                &tlv_forwarder,
                &clock,
            )
            .await;
        }

        let mut event_buffer = [0; MAX_DATA_LEN];
        let mut general_buffer = [0; 2048];

        loop {
            let mut actions = tokio::select! {
                result = event_socket.recv(&mut event_buffer) => match result {
                    Ok(packet) => {
                        if !is_message_buffer_compatible(&event_buffer[..packet.bytes_read]) {
                            // do not spam with missing timestamp error in mixed-version PTPv1+v2 networks
                            PortActionIterator::empty()
                        } else if let Some(timestamp) = packet.timestamp {
                            log::trace!("Recv timestamp: {:?}", packet.timestamp);
                            port.handle_event_receive(&event_buffer[..packet.bytes_read], clock.port_timestamp_to_time(timestamp))
                        } else {
                            log::error!("Missing recv timestamp");
                            PortActionIterator::empty()
                        }
                    }
                    Err(error) => panic!("Error receiving: {error:?}"),
                },
                result = general_socket.recv(&mut general_buffer) => match result {
                    Ok(packet) => port.handle_general_receive(&general_buffer[..packet.bytes_read]),
                    Err(error) => panic!("Error receiving: {error:?}"),
                },
                () = announce_notifier.notified() => {
                    port.handle_announce_timer(&mut tlv_forwarder)
                },
                () = sync_notifier.notified() => {
                    port.handle_sync_timer()
                },
                () = &mut timers.port_announce_timeout_timer => {
                    port.handle_announce_receipt_timer()
                },
                () = &mut timers.delay_request_timer => {
                    port.handle_delay_request_timer()
                },
                () = &mut timers.filter_update_timer => {
                    port.handle_filter_update_timer()
                },
                result = bmca_notify.wait_for(|v| *v) => match result {
                    Ok(_) => break,
                    Err(error) => panic!("Error on bmca notify: {error:?}"),
                }
            };

            loop {
                let pending_timestamp = handle_actions(
                    actions,
                    &mut event_socket,
                    &mut general_socket,
                    &mut timers,
                    &tlv_forwarder,
                    &clock,
                )
                .await;

                // there might be more actions to handle based on the current action
                actions = match pending_timestamp {
                    Some((context, timestamp)) => port.handle_send_timestamp(context, timestamp),
                    None => break,
                };
            }
        }

        let port_in_bmca = port.start_bmca();
        port_task_sender.send(port_in_bmca).await.unwrap();
    }
}

type BmcaPort = Port<
    'static,
    InBmca,
    Option<Vec<ClockIdentity>>,
    StdRng,
    BoxedClock,
    KalmanFilter,
    RwLock<PtpInstanceState>,
>;

struct Timers<'a> {
    port_announce_timeout_timer: Pin<&'a mut Timer>,
    delay_request_timer: Pin<&'a mut Timer>,
    filter_update_timer: Pin<&'a mut Timer>,
}

async fn handle_actions<A: NetworkAddress + PtpTargetAddress + Send>(
    actions: PortActionIterator<'_>,
    event_socket: &mut Socket<A>,
    general_socket: &mut Socket<A>,
    timers: &mut Timers<'_>,
    tlv_forwarder: &TlvForwarder,
    clock: &BoxedClock,
) -> Option<(TimestampContext, Time)> {
    let mut pending_timestamp = None;

    for action in actions {
        match action {
            PortAction::SendEvent {
                context,
                data,
                link_local,
            } => {
                // send timestamp of the send
                let time = event_socket
                    .send_to(
                        data,
                        if link_local {
                            A::PDELAY_EVENT
                        } else {
                            A::PRIMARY_EVENT
                        },
                    )
                    .await
                    .expect("Failed to send event message");

                // anything we send later will have a later pending (send) timestamp
                if let Some(time) = time {
                    log::trace!("Send timestamp {:?}", time);
                    pending_timestamp = Some((context, clock.port_timestamp_to_time(time)));
                } else {
                    log::error!("Missing send timestamp");
                }
            }
            PortAction::SendGeneral { data, link_local } => {
                general_socket
                    .send_to(
                        data,
                        if link_local {
                            A::PDELAY_GENERAL
                        } else {
                            A::PRIMARY_GENERAL
                        },
                    )
                    .await
                    .expect("Failed to send general message");
            }
            PortAction::ResetDelayRequestTimer { duration } => {
                timers.delay_request_timer.as_mut().reset(duration);
            }
            PortAction::ResetAnnounceReceiptTimer { duration } => {
                timers.port_announce_timeout_timer.as_mut().reset(duration);
            }
            PortAction::ResetFilterUpdateTimer { duration } => {
                timers.filter_update_timer.as_mut().reset(duration);
            }
            PortAction::ForwardTLV { tlv } => {
                tlv_forwarder.forward(tlv.into_owned());
            }
            _ => {}
        }
    }

    pending_timestamp
}
