//! Dev tool for exercising the clipboard watcher end-to-end without
//! wl-clipboard installed. Speaks the same ext-data-control protocol the
//! watcher consumes, so it also validates the compositor's server side.
//!
//! Usage:
//!   clipboard-tool get               # print the current text selection
//!   clipboard-tool set <text>        # own the selection until replaced/killed
//!   clipboard-tool set-image <path>  # own the selection with an image file
//!   clipboard-tool clear             # clear the selection

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsFd as _;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

const TEXT_MIME: &str = "text/plain;charset=utf-8";

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();

    let conn = Connection::connect_to_env().expect("connect to wayland display");
    let (globals, mut queue) = registry_queue_init::<ToolState>(&conn).expect("registry init");
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .expect("bind ext_data_control_manager_v1");
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).expect("bind wl_seat");
    let device = manager.get_data_device(&seat, &qh, ());
    let mut state = ToolState::default();

    match mode.as_str() {
        "get" => {
            queue.roundtrip(&mut state).expect("roundtrip");
            let Some(offer) = state.selection.take() else {
                return; // empty clipboard
            };
            let mimes = state.offers.remove(&offer.id()).unwrap_or_default();
            let Some(mime) = mimes
                .iter()
                .find(|m| m.eq_ignore_ascii_case(TEXT_MIME))
                .or_else(|| mimes.iter().find(|m| m.eq_ignore_ascii_case("text/plain")))
            else {
                return; // non-text clipboard
            };
            let (read_fd, write_fd) = rustix::pipe::pipe().expect("pipe");
            offer.receive(mime.clone(), write_fd.as_fd());
            drop(write_fd);
            conn.flush().expect("flush");
            let mut text = String::new();
            let _ = std::fs::File::from(read_fd).read_to_string(&mut text);
            print!("{text}");
        }
        "set" => {
            state.payload = args.next().expect("set needs a payload argument").into_bytes();
            let source = manager.create_data_source(&qh, ());
            source.offer(TEXT_MIME.to_string());
            source.offer("text/plain".to_string());
            source.offer("UTF8_STRING".to_string());
            device.set_selection(Some(&source));
            while !state.cancelled {
                queue.blocking_dispatch(&mut state).expect("dispatch");
            }
        }
        "set-image" => {
            let path = args.next().expect("set-image needs a file path");
            state.payload = std::fs::read(&path).expect("read image file");
            let mime = match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                other => panic!("unsupported image extension {other:?}"),
            };
            let source = manager.create_data_source(&qh, ());
            source.offer(mime.to_string());
            device.set_selection(Some(&source));
            while !state.cancelled {
                queue.blocking_dispatch(&mut state).expect("dispatch");
            }
        }
        "clear" => {
            device.set_selection(None);
            queue.roundtrip(&mut state).expect("roundtrip");
        }
        other => {
            eprintln!("usage: clipboard-tool get|set <text>|clear (got {other:?})");
            std::process::exit(2);
        }
    }
}

#[derive(Default)]
struct ToolState {
    offers: HashMap<ObjectId, Vec<String>>,
    selection: Option<ExtDataControlOfferV1>,
    payload: Vec<u8>,
    cancelled: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ToolState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ToolState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ToolState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ToolState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => state.selection = id,
            _ => {}
        }
    }

    event_created_child!(ToolState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for ToolState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for ToolState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_source_v1::Event;
        match event {
            Event::Send { mime_type: _, fd } => {
                let _ = std::fs::File::from(fd).write_all(&state.payload);
            }
            Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}
