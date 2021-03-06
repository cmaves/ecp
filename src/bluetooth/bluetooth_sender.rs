use super::{ecp_bufs, parse_time_signal, BMsg, BleOptions, Status, ECP_BUF1_BASE, ECP_UUID};
use crate::{Error, LedMsg, Sender};
use nix::poll::{poll, PollFd, PollFlags};
use rustable::gatt::{
    CharFlags, AttValue, LocalCharBase, LocalServiceBase, WriteType, ValOrFn, WritableAtt, HasChildren
};
use rustable::interfaces::BLUEZ_FAILED;
use rustable::{Bluetooth as BT, ToUUID};
use std::cell::{Cell, RefCell};
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::sync::mpsc::{sync_channel, SyncSender, TryRecvError};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

const ECP_TIME: &'static str = "79f4bb2c-7885-4584-8ef9-ae205b0eb345";

struct Bluetooth {
    verbose: u8,
    blue: BT,
    time: Rc<Cell<u32>>,
    last_set: Rc<Cell<Instant>>,
    msgs: Rc<RefCell<[Option<LedMsg>; 256]>>,
    last_sent: Rc<Cell<u32>>,
    wait: Rc<Cell<Duration>>,
}
impl Bluetooth {
    fn new(blue_path: String, verbose: u8) -> Result<Self, Error> {
        let mut blue = BT::new("io.maves.ecp_sender".to_string(), blue_path)?;
        blue.set_filter(None)?;
        blue.verbose = verbose;
        let mut ret = Bluetooth {
            verbose,
            blue,
            time: Rc::new(Cell::new(0)),
            last_set: Rc::new(Cell::new(Instant::now())),
            msgs: Rc::new(RefCell::new([None; 256])),
            last_sent: Rc::new(Cell::new(0)),
            wait: Rc::new(Cell::new(Duration::from_secs_f64(1.0 / 32.0))),
        };
        ret.init_service()?;
        Ok(ret)
    }

    fn init_service(&mut self) -> Result<(), Error> {
        let ecp_uuid = ECP_UUID.to_uuid();
        let mut sender_service = LocalServiceBase::new(&ecp_uuid, true);
        let mut flags = CharFlags::default();
        flags.broadcast = true;
        flags.read = true;
        flags.notify = true;
        let uuids = ecp_bufs();
        let mut notify_flags = flags;
        notify_flags.write_wo_response = true;
        let mut base = LocalCharBase::new(&uuids[0], notify_flags);
        base.enable_write_fd(true);
        let last_sent_clone = self.last_sent.clone();
        let wait_clone = self.wait.clone();
        let mut lat_total: i64 = 0;
        let mut lat_cnt = 0;
        let mut last_lat_total: i64 = 0;
        let verbose = self.verbose;
        base.write_callback = Some(Box::new(move |bytes| {
            eprintln!("Calling write_callback on: {:?}", bytes);
            if bytes.len() != 4 {
                return Err((BLUEZ_FAILED.to_string(), Some("Invalid length".to_string())));
            }
            let time = parse_time_signal(bytes);
            let last_sent = last_sent_clone.get();
            let diff = last_sent.wrapping_sub(time);
            lat_cnt += 1;
            lat_total += diff as i64;
            if lat_cnt >= 32 {
                let lat_growth = (lat_total as i64) - (last_lat_total as i64);
                let (mult, verb_str) = if lat_growth <= 0 {
                    (31.0 / 32.0, "reducing wait interval")
                } else {
                    (40.0 / 32.0, "increasing wait interval")
                };
                if verbose >= 3 {
                    eprintln!("lat_growth: {}, {}", lat_growth, verb_str);
                }
				let dur = wait_clone.get().mul_f64(mult).min(Duration::from_millis(500));
                wait_clone.set(dur);
                lat_cnt = 0;
                last_lat_total = lat_total;
                lat_total = 0;
            }
            Ok((None, false))
        }));
        sender_service.add_char(base);
        for uuid in &uuids[1..] {
            let mut base = LocalCharBase::new(uuid, flags);
            base.notify_fd_buf = Some(256);
            sender_service.add_char(base);
        }
        self.blue.add_service(sender_service)?;
        let mut sender_service = self.blue.get_service(ecp_uuid).unwrap();
        for (i, uuid) in uuids[1..5].iter().enumerate() {
            let rc_msgs = self.msgs.clone();
            let read_fn = move || {
                let start = i * 64;
                let end = start + 64;
                let mut cv = AttValue::new(512);
                let mut msgs = [LedMsg::default(); 64];
                let mut cnt = 0;
                let borrow = rc_msgs.borrow();
                let iter = borrow[start..end].iter().filter_map(|x| *x);
                for (dst, src) in msgs.iter_mut().zip(iter) {
                    *dst = src;
                    cnt += 1;
                }
				let msg_buf = &msgs[..cnt];
				let time = msg_buf.get(0).map_or(0, |msg| msg.cur_time);
                let (len, msgs_consumed) = LedMsg::serialize(&msgs[..cnt], cv.as_mut_slice(), Some(time));
                debug_assert_eq!(msgs_consumed, cnt);
                cv.resize(len, 0);
                cv
            };
            let mut ecp_char = sender_service.get_child(uuid).unwrap();
            ecp_char.write_val_or_fn(&mut ValOrFn::Function(Box::new(read_fn)));
        }
        let time = self.time.clone();
        let last_set = self.last_set.clone();
        let time_closure = move || {
            time_fn(time.get(), last_set.get(), Instant::now())
                .to_be_bytes()
                .as_ref()
                .into()
        };
        let mut time_serv = sender_service.get_child(&uuids[5]).unwrap();
        time_serv.write_val_or_fn(&mut ValOrFn::Function(Box::new(time_closure)));
        self.blue.register_application()?;
        Ok(())
    }
}
fn time_fn(time: u32, last_set: Instant, now: Instant) -> u32 {
    time.wrapping_add(now.duration_since(last_set).as_micros() as u32)
}
pub struct BluetoothSender {
    sender: SyncSender<BMsg>,
    handle: Status,
}

fn process_requests(dur: Duration, bt: &mut Bluetooth) -> Result<(), Error> {
    let target = Instant::now() + dur;
    let bt_fd = bt.blue.as_raw_fd();
    let mut ecp_serv = bt.blue.get_service(ECP_UUID).unwrap();
    let notify_char = ecp_serv.get_child(ECP_BUF1_BASE).unwrap();
    let notify_fd = match notify_char.get_write_fd() {
        Some(fd) => fd,
        None => -1,
    };
    let mut polls = [
        PollFd::new(notify_fd, PollFlags::POLLIN),
        PollFd::new(bt_fd, PollFlags::POLLIN),
    ];
    let mut sleep_time = target.saturating_duration_since(Instant::now()).as_millis();
    loop {
        if let Ok(i) = poll(&mut polls, sleep_time as i32) {
            if i > 0 {
                let evts = polls[0].revents().unwrap();
                if !evts.is_empty() {
                    let mut ecp_serv = bt.blue.get_service(ECP_UUID).unwrap();
                    let mut notify_char = ecp_serv.get_child(ECP_BUF1_BASE).unwrap();
                    notify_char.check_write_fd()?;
                }
                let evts = polls[1].revents().unwrap();
                if !evts.is_empty() {
                    bt.blue.process_requests()?;
                }
            }
        }
        match target.checked_duration_since(Instant::now()) {
            Some(sleep) => sleep_time = sleep.as_millis(),
            None => break,
        }
    }
    Ok(())
}
impl BluetoothSender {
    pub fn new(blue_path: String, options: BleOptions) -> Result<Self, Error> {
        let (sender, recv) = sync_channel(1);
        let handle = Status::Running(spawn(move || {
            let mut bt = Bluetooth::new(blue_path, options.verbose)?;
            let ecp_bufs = ecp_bufs();
            let mut last_notify_time = Instant::now();

            // the stats data
            let target_dur = Duration::from_secs(options.stats.into());
            let stats_start_total = Instant::now();
            let mut stats_period_start = stats_start_total;
            let mut sent_pkts_cnt = 0;
            let mut sent_pkts_cnt_total = 0;
            let mut sent_bytes = 0;
            let mut sent_bytes_total = 0;
            loop {
                process_requests(bt.wait.get(), &mut bt)?;
                match recv.try_recv() {
                    Ok(msg) => match msg {
                        BMsg::SendMsg(msgs, start) => {
                            if msgs.len() == 0 {
                                continue;
                            }
                            let now = Instant::now();
                            let old_time = time_fn(bt.time.get(), bt.last_set.get(), now);
                            let cur_time = time_fn(msgs[0].cur_time, start, now);

                            bt.last_set.set(now);
                            bt.time.set(cur_time);
                            let notify_time = (cur_time.wrapping_sub(old_time) as i32).abs()
                                > 5_000
                                || now.duration_since(last_notify_time).as_millis() > 5_000;

                            let mut mut_msgs = bt.msgs.borrow_mut();
                            for msg in &msgs {
                                mut_msgs[msg.element as usize] = Some(*msg);
                            }
                            for msg in mut_msgs.iter_mut() {
                                // prune old messages
                                if let Some(v) = msg {
                                    if (cur_time.wrapping_sub(v.cur_time) as i32).abs() > 5_000_000
                                    {
                                        // the i32::abs allows values that are up to 5 seconds early to array
                                        *msg = None;
                                    }
                                }
                            }
                            drop(mut_msgs);

                            // eprintln!("dirty received: {:?}", dirty);
                            // write out the dirty characteristics and
                            let mut service = bt.blue.get_service(ECP_UUID).unwrap();
                            if notify_time {
                                let mut character = service.get_child(ECP_TIME).unwrap();
                                character.notify()?;
                                last_notify_time = now;
                            }
                            let mut notify_char = service.get_child(&ecp_bufs[0]).unwrap();
                            let mtu = notify_char.get_notify_mtu().unwrap_or(23) - 3; // The 3 accounts for ATT HDR
                            let mut written = 0;
							let sent_time = msgs[0].cur_time;
							bt.last_sent.set(sent_time);
                            while written < msgs.len() {
                                let mut cv = AttValue::new(mtu as usize);

                                let (len, consumed) =
                                    LedMsg::serialize(&msgs[written..], cv.as_mut_slice(), Some(sent_time));
                                cv.resize(len, 0);
                                written += consumed;
                                notify_char.write_wait(cv, WriteType::WithoutRes)?;
                                if let Err(e) = notify_char.notify() {
                                    if let rustable::Error::Timeout = e {
                                    } else {
                                        return Err(e.into());
                                    }
                                }
                                if options.stats != 0 {
                                    sent_pkts_cnt += 1;
                                    sent_pkts_cnt_total += 1;
                                    sent_bytes += len;
                                    sent_bytes_total += len;
                                }
                            }
                            if options.stats != 0 {
                                let now = Instant::now();
                                let since = now.duration_since(stats_period_start);
                                if since > target_dur {
                                    let since_secs = since.as_secs_f64();
                                    eprintln!("Sending stats:\n\tPeriod throughput: {:.1} Msgs/s, {:.0} msgs, {:.0} Bps, {} bytes, Avg size: {:.0} bytes", sent_pkts_cnt as f64 / since_secs, sent_pkts_cnt, sent_bytes as f64 / since_secs, sent_bytes, sent_bytes / sent_pkts_cnt);

                                    let since_secs_total =
                                        now.duration_since(stats_start_total).as_secs_f64();

                                    eprintln!("\tTotal throughput: {:.1} Msgs/s, {:.0} msgs, {:.0} Bps, {} bytes, Avg size: {:.0} bytes\n", sent_pkts_cnt_total as f64 / since_secs_total, sent_pkts_cnt_total, sent_bytes_total as f64 / since_secs_total, sent_bytes_total, sent_bytes_total / sent_pkts_cnt_total);

                                    // reset period stats
                                    stats_period_start = now;
                                    sent_pkts_cnt = 0;
                                    sent_bytes = 0;
                                }
                            }
                        }
                        BMsg::Terminate => return Ok(()),
                        BMsg::Alive => (),
                    },
                    Err(e) => {
                        if let TryRecvError::Disconnected = e {
                            return Err(Error::Unrecoverable(
                                "BT sender thread: Msg channel disconnected! exiting..."
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
        }));
        sleep(Duration::from_millis(500));
        let ret = BluetoothSender { sender, handle };
        ret.is_alive();
        if ret.is_alive() {
            Ok(ret)
        } else {
            Err(ret.terminate().unwrap_err())
        }
    }
    pub fn is_alive(&self) -> bool {
        self.sender.send(BMsg::Alive).is_ok()
    }
    pub fn terminate(self) -> Result<(), Error> {
        self.sender.send(BMsg::Terminate).ok();
        match self.handle {
            Status::Running(handle) => match handle.join() {
                Ok(ret) => ret,
                Err(err) => Err(Error::Unrecoverable(format!(
                    "DBus bluetooth thread panicked with: {:?}",
                    err
                ))),
            },
            Status::Terminated => Err(Error::BadInput("Thread already terminated".to_string())),
        }
    }
}

impl Sender for BluetoothSender {
    fn send(&mut self, msgs: &[LedMsg]) -> Result<(), Error> {
        let start = Instant::now();
        let msg_vec = Vec::from(msgs);
        match self.sender.send(BMsg::SendMsg(msg_vec, start)) {
            Ok(()) => Ok(()),
            Err(_) => match self.handle {
                Status::Running(_) => {
                    let mut handle = Status::Terminated;
                    std::mem::swap(&mut handle, &mut self.handle);
                    match handle {
                        Status::Running(handle) => handle.join().unwrap(),
                        Status::Terminated => unreachable!(),
                    }
                }
                Status::Terminated => Err(Error::Unrecoverable(
                    "BluetoothSender: Sending thread is disconnected!".to_string(),
                )),
            },
        }
    }
}
