use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use config::{Config, Value};
use derive_builder::Builder;
use pangocairo::functions::{create_layout, show_layout};
use tokio::task::{self, JoinHandle};
use tokio_stream::{Stream, StreamExt};
use xcb::{x, XidNew};

use crate::{
    bar::PanelDrawInfo, remove_string_from_config, remove_uint_from_config,
    x::intern_named_atom, Attrs, Highlight, PanelCommon, PanelConfig,
    PanelStream,
};

struct XStream {
    conn: Arc<xcb::Connection>,
    number_atom: x::Atom,
    current_atom: x::Atom,
    names_atom: x::Atom,
    handle: Option<JoinHandle<()>>,
}

impl XStream {
    const fn new(
        conn: Arc<xcb::Connection>,
        number_atom: x::Atom,
        current_atom: x::Atom,
        names_atom: x::Atom,
    ) -> Self {
        Self {
            conn,
            number_atom,
            current_atom,
            names_atom,
            handle: None,
        }
    }
}

impl Stream for XStream {
    type Item = ();

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(handle) = &self.handle {
            if handle.is_finished() {
                self.handle = None;
                Poll::Ready(Some(()))
            } else {
                Poll::Pending
            }
        } else {
            let conn = self.conn.clone();
            let waker = cx.waker().clone();
            let number_atom = self.number_atom;
            let current_atom = self.current_atom;
            let names_atom = self.names_atom;
            self.handle = Some(task::spawn_blocking(move || loop {
                let event = conn.wait_for_event();
                if let Ok(xcb::Event::X(x::Event::PropertyNotify(event))) =
                    event
                {
                    if event.atom() == number_atom
                        || event.atom() == current_atom
                        || event.atom() == names_atom
                    {
                        waker.wake();
                        break;
                    }
                }
            }));
            Poll::Pending
        }
    }
}

/// Display information about workspaces
///
/// Requires an EWMH-compliant window manager
#[derive(Clone, Builder)]
#[builder_struct_attr(allow(missing_docs))]
#[builder_impl_attr(allow(missing_docs))]
pub struct XWorkspaces {
    conn: Arc<xcb::Connection>,
    screen: i32,
    #[builder(default = "0")]
    padding: i32,
    #[builder(setter(strip_option))]
    highlight: Option<Highlight>,
    common: PanelCommon,
}

impl XWorkspaces {
    fn draw(
        &self,
        cr: &Rc<cairo::Context>,
        root: x::Window,
        height: i32,
        number_atom: x::Atom,
        names_atom: x::Atom,
        utf8_atom: x::Atom,
        current_atom: x::Atom,
        client_atom: x::Atom,
        type_atom: x::Atom,
        normal_atom: x::Atom,
        desktop_atom: x::Atom,
    ) -> Result<PanelDrawInfo> {
        let workspaces = get_workspaces(
            &self.conn,
            root,
            number_atom,
            names_atom,
            utf8_atom,
        )?;
        let current = get_current(&self.conn, root, current_atom)?;
        let nonempty_set = get_nonempty(
            &self.conn,
            root,
            client_atom,
            type_atom,
            normal_atom,
            desktop_atom,
        )?;

        // TODO: avoid cloning?
        let nonempty_set2 = nonempty_set.clone();

        let active = self.common.attrs[0].clone();
        let nonempty = self.common.attrs[1].clone();
        let inactive = self.common.attrs[2].clone();
        let layouts: Vec<_> = workspaces
            .into_iter()
            .enumerate()
            .map(move |(i, w)| {
                let i = i as u32;
                let layout = create_layout(cr);
                if i == current {
                    active.apply_font(&layout);
                } else if nonempty_set2.contains(&i) {
                    nonempty.apply_font(&layout);
                } else {
                    inactive.apply_font(&layout);
                }
                layout.set_text(w.as_str());
                (i, layout)
            })
            .collect();

        let width = layouts
            .iter()
            .map(|l| l.1.pixel_size().0 + self.padding)
            .sum::<i32>()
            - self.padding;

        let padding = self.padding;
        let active = self.common.attrs[0].clone();
        let nonempty = self.common.attrs[1].clone();
        let inactive = self.common.attrs[2].clone();
        let highlight = self.highlight.clone();

        Ok(PanelDrawInfo::new(
            (width, height),
            self.common.dependence,
            Box::new(move |cr| {
                for (i, layout) in &layouts {
                    if *i == current {
                        active.apply_bg(cr);
                    } else if nonempty_set.contains(i) {
                        nonempty.apply_bg(cr);
                    } else {
                        inactive.apply_bg(cr);
                    }

                    let size = layout.pixel_size();

                    cr.save()?;
                    cr.rectangle(
                        0.0,
                        0.0,
                        f64::from(size.0 + padding),
                        f64::from(height),
                    );
                    cr.fill()?;

                    if *i == current {
                        if let Some(highlight) = &highlight {
                            cr.rectangle(
                                0.0,
                                f64::from(height) - highlight.height,
                                f64::from(size.0 + padding),
                                highlight.height,
                            );
                            cr.set_source_rgba(
                                highlight.color.r,
                                highlight.color.g,
                                highlight.color.b,
                                highlight.color.a,
                            );
                            cr.fill()?;
                        }
                    }

                    cr.translate(
                        f64::from(padding / 2),
                        f64::from(height - size.1) / 2.0,
                    );

                    if *i == current {
                        active.apply_fg(cr);
                    } else if nonempty_set.contains(i) {
                        nonempty.apply_fg(cr);
                    } else {
                        inactive.apply_fg(cr);
                    }

                    show_layout(cr, layout);
                    cr.restore()?;

                    cr.translate(
                        f64::from(layout.pixel_size().0 + padding),
                        0.0,
                    );
                }
                Ok(())
            }),
        ))
    }
}

impl PanelConfig for XWorkspaces {
    fn into_stream(
        mut self: Box<Self>,
        cr: Rc<cairo::Context>,
        global_attrs: Attrs,
        height: i32,
    ) -> Result<PanelStream> {
        let number_atom =
            intern_named_atom(&self.conn, b"_NET_NUMBER_OF_DESKTOPS")?;
        let names_atom = intern_named_atom(&self.conn, b"_NET_DESKTOP_NAMES")?;
        let utf8_atom = intern_named_atom(&self.conn, b"UTF8_STRING")?;
        let current_atom =
            intern_named_atom(&self.conn, b"_NET_CURRENT_DESKTOP")?;
        let client_atom = intern_named_atom(&self.conn, b"_NET_CLIENT_LIST")?;
        let type_atom = intern_named_atom(&self.conn, b"_NET_WM_WINDOW_TYPE")?;
        let normal_atom =
            intern_named_atom(&self.conn, b"_NET_WM_WINDOW_TYPE_NORMAL")?;
        let desktop_atom = intern_named_atom(&self.conn, b"_NET_WM_DESKTOP")?;

        let root = self
            .conn
            .get_setup()
            .roots()
            .nth(self.screen as usize)
            .ok_or_else(|| anyhow!("Screen not found"))?
            .root();
        self.conn.check_request(self.conn.send_request_checked(
            &x::ChangeWindowAttributes {
                window: root,
                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
            },
        ))?;

        for attr in &mut self.common.attrs {
            attr.apply_to(&global_attrs);
        }

        let stream = tokio_stream::once(())
            .chain(XStream::new(
                self.conn.clone(),
                number_atom,
                current_atom,
                names_atom,
            ))
            .map(move |_| {
                self.draw(
                    &cr,
                    root,
                    height,
                    number_atom,
                    names_atom,
                    utf8_atom,
                    current_atom,
                    client_atom,
                    type_atom,
                    normal_atom,
                    desktop_atom,
                )
            });
        Ok(Box::pin(stream))
    }

    /// Configuration options:
    ///
    /// - `screen`: the name of the X screen to monitor
    ///   - type: String
    ///   - default: None (This will tell X to choose the default screen, which
    ///     is probably what you want.)
    ///
    /// - `padding`: The space in pixels between two workspace names. The
    ///   [`Attrs`] will change (if applicable) halfway between the two names.
    ///   - type: u64
    ///   - default: 0
    ///
    /// - `highlight`: The highlight that will appear on the active workspaces.
    ///   See [`Highlight::parse`] for parsing options.
    ///
    /// - See [`PanelCommon::parse`]. No format strings are used for this panel.
    ///   Three instances of [`Attrs`] are parsed using the prefixes `active_`,
    ///   `nonempty_`, and `inactive_`
    fn parse(
        table: &mut HashMap<String, Value>,
        _global: &Config,
    ) -> Result<Self> {
        let mut builder = XWorkspacesBuilder::default();
        let screen = remove_string_from_config("screen", table);
        if let Ok((conn, screen)) = xcb::Connection::connect(screen.as_deref())
        {
            builder.conn(Arc::new(conn)).screen(screen);
        } else {
            log::error!("Failed to connect to X server");
        }
        if let Some(padding) = remove_uint_from_config("padding", table) {
            builder.padding(padding as i32);
        }

        builder.common(PanelCommon::parse(
            table,
            &[],
            &[],
            &["active_", "nonempty_", "inactive_"],
        )?);

        builder.highlight(Highlight::parse(table));

        Ok(builder.build()?)
    }
}

fn get_workspaces(
    conn: &xcb::Connection,
    root: x::Window,
    number_atom: x::Atom,
    names_atom: x::Atom,
    utf8_atom: x::Atom,
) -> Result<Vec<String>> {
    let number: u32 = conn
        .wait_for_reply(conn.send_request(&x::GetProperty {
            delete: false,
            window: root,
            property: number_atom,
            r#type: x::ATOM_CARDINAL,
            long_offset: 0,
            long_length: 1,
        }))?
        .value()[0];

    let reply = conn.wait_for_reply(conn.send_request(&x::GetProperty {
        delete: false,
        window: root,
        property: names_atom,
        r#type: utf8_atom,
        long_offset: 0,
        long_length: number,
    }))?;
    let bytes: &[u8] = reply.value();

    let mut names: Vec<String> = bytes
        .split(|&b| b == 0)
        .map(|s| unsafe { String::from_utf8_unchecked(s.to_vec()) })
        .collect();

    if names.len() < number as usize {
        names.extend(vec![String::from("?"); number as usize - names.len()]);
    }

    Ok(names)
}

fn get_current(
    conn: &xcb::Connection,
    root: x::Window,
    current_atom: x::Atom,
) -> Result<u32> {
    Ok(conn
        .wait_for_reply(conn.send_request(&x::GetProperty {
            delete: false,
            window: root,
            property: current_atom,
            r#type: x::ATOM_CARDINAL,
            long_offset: 0,
            long_length: 1,
        }))?
        .value()[0])
}

fn get_nonempty(
    conn: &xcb::Connection,
    root: x::Window,
    client_atom: x::Atom,
    type_atom: x::Atom,
    normal_atom: x::Atom,
    desktop_atom: x::Atom,
) -> Result<HashSet<u32>> {
    Ok(get_clients(conn, root, client_atom)?
        .iter()
        .filter(|&&w| {
            conn.wait_for_reply(conn.send_request(&x::GetProperty {
                delete: false,
                window: w,
                property: type_atom,
                r#type: x::ATOM_ATOM,
                long_offset: 0,
                long_length: 1,
            }))
            .map_or(false, |r| r.value::<x::Atom>()[0] == normal_atom)
        })
        .filter_map(|&w| {
            conn.wait_for_reply(conn.send_request(&x::GetProperty {
                delete: false,
                window: w,
                property: desktop_atom,
                r#type: x::ATOM_CARDINAL,
                long_offset: 0,
                long_length: 1,
            }))
            .ok()
        })
        .map(|r| r.value::<u32>()[0])
        .collect())
}

fn get_clients(
    conn: &xcb::Connection,
    root: x::Window,
    client_atom: x::Atom,
) -> Result<Vec<x::Window>> {
    let mut windows = Vec::new();

    loop {
        let reply =
            conn.wait_for_reply(conn.send_request(&x::GetProperty {
                delete: false,
                window: root,
                property: client_atom,
                r#type: x::ATOM_WINDOW,
                long_offset: windows.len() as u32,
                long_length: 16,
            }))?;

        let wids: Vec<u32> = reply.value().to_vec();
        windows.append(
            &mut wids.iter().map(|&w| unsafe { x::Window::new(w) }).collect(),
        );

        if reply.bytes_after() == 0 {
            break;
        }
    }

    Ok(windows)
}
