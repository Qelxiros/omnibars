use std::{
    collections::HashMap,
    pin::Pin,
    rc::Rc,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use config::{Config, Value};
use derive_builder::Builder;
use futures::FutureExt;
use libpulse_binding::{
    callbacks::ListResult,
    context::{self, subscribe::InterestMaskSet, FlagSet, State},
    mainloop::threaded,
    volume::Volume,
};
use pangocairo::functions::show_layout;
use tokio::task::{self, JoinHandle};
use tokio_stream::{Stream, StreamExt};

use crate::{Attrs, PanelConfig, PanelDrawFn, PanelStream, Ramp};

#[derive(Builder)]
pub struct Pulseaudio {
    #[builder(default = r#"String::from("@DEFAULT_SINK@")"#)]
    sink: String,
    #[builder(default, setter(strip_option))]
    server: Option<String>,
    #[builder(default, setter(strip_option))]
    ramp: Option<Ramp>,
    #[builder(default, setter(strip_option))]
    muted_ramp: Option<Ramp>,
    attrs: Attrs,
    send: Sender<(Volume, bool)>,
    recv: Arc<Mutex<Receiver<(Volume, bool)>>>,
    #[builder(default, setter(skip))]
    handle: Option<JoinHandle<Result<(Volume, bool)>>>,
}

impl Stream for Pulseaudio {
    type Item = (Volume, bool);

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(handle) = &mut self.handle {
            if handle.is_finished() {
                let value = handle
                    .poll_unpin(cx)
                    .map(|r| r.map(Result::ok).ok().flatten());
                if let Poll::Ready(_) = value {
                    self.handle = None;
                }
                value
            } else {
                Poll::Pending
            }
        } else {
            let waker = cx.waker().clone();
            let recv = self.recv.clone();
            self.handle = Some(task::spawn_blocking(move || {
                let value = recv.lock().unwrap().recv()?;
                waker.wake_by_ref();
                Ok(value)
            }));
            Poll::Pending
        }
    }
}

impl Pulseaudio {
    fn draw(
        cr: &Rc<cairo::Context>,
        data: (Volume, bool),
        ramp: Option<&Ramp>,
        muted_ramp: Option<&Ramp>,
        attrs: &Attrs,
    ) -> ((i32, i32), PanelDrawFn) {
        let (volume, mute) = data;
        let ramp = match (mute, muted_ramp) {
            (false, _) | (true, None) => ramp,
            (true, Some(_)) => muted_ramp,
        };
        let prefix = ramp
            .as_ref()
            .map(|r| r.choose(volume.0, Volume::MUTED.0, Volume::NORMAL.0));
        let text = format!(
            "{}{}",
            prefix.unwrap_or(String::new()),
            volume.to_string().as_str()
        );
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_markup(text.as_str());
        attrs.apply_font(&layout);
        let dims = layout.pixel_size();
        let attrs = attrs.clone();

        (
            dims,
            Box::new(move |cr| {
                attrs.apply_bg(cr);
                cr.rectangle(0.0, 0.0, f64::from(dims.0), f64::from(dims.1));
                cr.fill()?;
                attrs.apply_fg(cr);
                show_layout(cr, &layout);
                Ok(())
            }),
        )
    }
}

impl Default for Pulseaudio {
    fn default() -> Self {
        let (send, recv) = channel();
        Self {
            sink: String::from("@DEFAULT_SINK@"),
            server: None,
            ramp: None,
            muted_ramp: None,
            attrs: Attrs::default(),
            send,
            recv: Arc::new(Mutex::new(recv)),
            handle: None,
        }
    }
}

impl PanelConfig for Pulseaudio {
    fn into_stream(
        mut self: Box<Self>,
        cr: Rc<cairo::Context>,
        global_attrs: Attrs,
        _height: i32,
    ) -> Result<PanelStream> {
        let mut mainloop = threaded::Mainloop::new()
            .ok_or_else(|| anyhow!("Failed to create pulseaudio mainloop"))?;
        mainloop.start()?;
        let mut context = context::Context::new(&mainloop, "omnibars")
            .ok_or_else(|| anyhow!("Failed to create pulseaudio context"))?;
        context.connect(self.server.as_deref(), FlagSet::NOFAIL, None)?;
        while context.get_state() != State::Ready {}
        let introspector = context.introspect();

        let (send, recv) = channel();
        self.send = send.clone();
        self.recv = Arc::new(Mutex::new(recv));
        let sink = self.sink.clone();

        mainloop.lock();

        let initial = send.clone();
        introspector.get_sink_info_by_name(sink.as_str(), move |r| {
            if let ListResult::Item(s) = r {
                let volume = s.volume.get()[0];
                let mute = s.mute;
                initial.send((volume, mute)).unwrap();
            }
        });

        context.subscribe(InterestMaskSet::SINK, |_| {});

        let cb: Option<Box<dyn FnMut(_, _, _)>> =
            Some(Box::new(move |_, _, _| {
                let send = send.clone();
                introspector.get_sink_info_by_name(sink.as_str(), move |r| {
                    if let ListResult::Item(s) = r {
                        let volume = s.volume.get()[0];
                        let mute = s.mute;
                        send.send((volume, mute)).unwrap();
                    }
                });
            }));

        context.set_subscribe_callback(cb);

        mainloop.unlock();

        // prevent these structures from going out of scope
        Box::leak(Box::new(context));
        Box::leak(Box::new(mainloop));

        self.attrs = global_attrs.overlay(self.attrs);
        let ramp = self.ramp.clone();
        let muted_ramp = self.muted_ramp.clone();
        let attrs = self.attrs.clone();

        let stream = self.map(move |data| {
            Ok(Self::draw(
                &cr,
                data,
                ramp.as_ref(),
                muted_ramp.as_ref(),
                &attrs,
            ))
        });

        Ok(Box::pin(stream))
    }

    fn parse(
        table: &mut HashMap<String, Value>,
        global: &Config,
    ) -> Result<Self> {
        let mut builder = PulseaudioBuilder::default();
        if let Some(sink) = table.remove("sink") {
            if let Ok(sink) = sink.clone().into_string() {
                builder.sink(sink);
            } else {
                log::warn!(
                    "Ignoring non-string value {sink:?} (location attempt: \
                     {:?})",
                    sink.origin()
                );
            }
        }
        if let Some(server) = table.remove("server") {
            if let Ok(server) = server.clone().into_string() {
                builder.server(server);
            } else {
                log::warn!(
                    "Ignoring non-string value {server:?} (location attempt: \
                     {:?})",
                    server.origin()
                );
            }
        }
        if let Some(ramp) = table.remove("ramp") {
            if let Ok(ramp) = ramp.clone().into_string() {
                if let Some(ramp) = Ramp::parse(ramp.as_str(), global) {
                    builder.ramp(ramp);
                } else {
                    log::warn!("Invalid ramp {ramp}");
                }
            } else {
                log::warn!(
                    "Ignoring non-string value {ramp:?} (location attempt: \
                     {:?})",
                    ramp.origin()
                );
            }
        }
        if let Some(muted_ramp) = table.remove("muted_ramp") {
            if let Ok(muted_ramp) = muted_ramp.clone().into_string() {
                if let Some(muted_ramp) =
                    Ramp::parse(muted_ramp.as_str(), global)
                {
                    builder.muted_ramp(muted_ramp);
                } else {
                    log::warn!("Invalid muted_ramp {muted_ramp}");
                }
            } else {
                log::warn!(
                    "Ignoring non-string value {muted_ramp:?} (location \
                     attempt: {:?})",
                    muted_ramp.origin()
                );
            }
        }

        let (send, recv) = channel();
        builder.send(send);
        builder.recv(Arc::new(Mutex::new(recv)));
        builder.attrs(Attrs::parse(table, ""));

        Ok(builder.build()?)
    }
}