//! The X11 capture backend.
//!
//! On X11 every capability Scrozz needs is genuinely available, so this backend
//! is complete rather than best-effort: full window enumeration with titles and
//! applications, per-monitor geometry from RandR, work areas that exclude
//! panels, and pixels from `GetImage`.
//!
//! The submodules split along one line: everything that is arithmetic or parsing
//! lives in [`ewmh`], [`pixels`], [`layout`], [`scale`] and [`wire`], which
//! depend on nothing but `std` and `scrozz-core` and are unit-tested on any host;
//! this file is the part that needs a server and therefore cannot be.
//!
//! # MIT-SHM
//!
//! The shared-memory path is detected and reported but not used. It needs both
//! `x11rb`'s `shm` feature and a way to call `shmget`/`shmat` or `memfd_create`
//! — that is, `libc` or `rustix` — and this crate's manifest grants neither.
//! `GetImage` is used instead, which is correct everywhere and slower on large
//! captures. [`X11Backend::shm_available`] records what was found so the report
//! is honest about which path ran.

pub mod ewmh;
pub mod layout;
pub mod pixels;
pub mod randr;
pub mod scale;
pub(crate) mod scroll;
pub mod wire;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, ColorSpace, Display, DisplayId, Error,
    Frame, LogicalRect, PhysicalSize, Provenance, Result, ScaleFactor, TargetEnumerator, Window,
    WindowId,
};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    self, AtomEnum, ImageFormat, ImageOrder, MapState, Screen, Visualtype,
};
use x11rb::rust_connection::RustConnection;

use self::layout::PixelRect;
use self::randr::RandrExtension;

/// Every atom the backend needs, interned once at connect time.
///
/// Interning is a round trip each; doing it lazily inside `windows()` would add
/// a dozen synchronous round trips to an operation that runs on every keystroke
/// in the picker.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    net_client_list: u32,
    net_client_list_stacking: u32,
    net_wm_name: u32,
    net_workarea: u32,
    net_current_desktop: u32,
    net_frame_extents: u32,
    net_active_window: u32,
    wm_state: u32,
    utf8_string: u32,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let names: [&[u8]; 9] = [
            b"_NET_CLIENT_LIST",
            b"_NET_CLIENT_LIST_STACKING",
            b"_NET_WM_NAME",
            b"_NET_WORKAREA",
            b"_NET_CURRENT_DESKTOP",
            b"_NET_FRAME_EXTENTS",
            b"_NET_ACTIVE_WINDOW",
            b"WM_STATE",
            b"UTF8_STRING",
        ];

        // Pipelined deliberately: nine requests then nine replies is one round
        // trip, nine request/reply pairs is nine.
        let cookies = names
            .iter()
            .map(|name| xproto::intern_atom(conn, false, name).map_err(platform))
            .collect::<Result<Vec<_>>>()?;

        let mut atoms = [0u32; 9];
        for (slot, cookie) in atoms.iter_mut().zip(cookies) {
            *slot = cookie.reply().map_err(platform)?.atom;
        }

        Ok(Self {
            net_client_list: atoms[0],
            net_client_list_stacking: atoms[1],
            net_wm_name: atoms[2],
            net_workarea: atoms[3],
            net_current_desktop: atoms[4],
            net_frame_extents: atoms[5],
            net_active_window: atoms[6],
            wm_state: atoms[7],
            utf8_string: atoms[8],
        })
    }
}

/// Still capture through a direct X11 connection.
pub struct X11Backend {
    conn: RustConnection,
    screen_index: usize,
    root: u32,
    atoms: Atoms,
    randr: Option<RandrExtension>,
    scale: ScaleFactor,
    shm_available: bool,
    image_lsb_first: bool,
    name: String,
}

impl std::fmt::Debug for X11Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Backend")
            .field("root", &self.root)
            .field("randr", &self.randr.map(|r| r.version()))
            .field("scale", &self.scale.get())
            .field("shm_available", &self.shm_available)
            .finish_non_exhaustive()
    }
}

impl X11Backend {
    /// Opens a connection to the X server named by `DISPLAY`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if no server is reachable.
    pub fn connect() -> Result<Self> {
        let (conn, screen_index) = x11rb::connect(None)
            .map_err(|err| Error::Platform(format!("could not connect to the X server: {err}")))?;

        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_index)
            .ok_or_else(|| Error::Platform("the X server reported no screens".into()))?;
        let root = screen.root;
        let image_lsb_first = setup.image_byte_order == ImageOrder::LSB_FIRST;

        let atoms = Atoms::intern(&conn)?;

        // A failure here is not fatal: a server without RandR still has one
        // screen, and one display is a better answer than no backend at all.
        let randr = RandrExtension::query(&conn).unwrap_or_else(|err| {
            tracing::warn!(%err, "RandR unavailable; falling back to a single display");
            None
        });

        let shm_available = conn
            .extension_information("MIT-SHM")
            .ok()
            .flatten()
            .is_some();

        let scale = read_scale(&conn, root);

        let name = format!(
            "X11 (GetImage{}{})",
            if randr.is_some() { ", RandR 1.5" } else { "" },
            if shm_available {
                ", MIT-SHM present but unused"
            } else {
                ""
            }
        );

        Ok(Self {
            conn,
            screen_index,
            root,
            atoms,
            randr,
            scale,
            shm_available,
            image_lsb_first,
            name,
        })
    }

    /// Whether the server offers MIT-SHM.
    ///
    /// Reported rather than used; see the module documentation.
    #[must_use]
    pub const fn shm_available(&self) -> bool {
        self.shm_available
    }

    fn screen(&self) -> Result<&Screen> {
        self.conn
            .setup()
            .roots
            .get(self.screen_index)
            .ok_or_else(|| Error::Platform("the X screen disappeared".into()))
    }

    /// Reads a window property in one round trip, capped at 1 MiB.
    ///
    /// The cap matters: `_NET_WM_NAME` is short but `_NET_CLIENT_LIST` on a
    /// heavily-populated desktop is not, and an unbounded read is an unbounded
    /// allocation driven by another process.
    fn property(&self, window: u32, property: u32, type_: u32) -> Option<(u32, Vec<u8>)> {
        if property == 0 {
            return None;
        }
        let reply = xproto::get_property(&self.conn, false, window, property, type_, 0, 262_144)
            .ok()?
            .reply()
            .ok()?;
        (reply.type_ != 0).then_some((reply.type_, reply.value))
    }

    fn monitors(&self) -> Result<Vec<(String, PixelRect, bool)>> {
        let screen = self.screen()?;

        let Some(randr) = self.randr else {
            return Ok(vec![(
                "Screen".to_owned(),
                PixelRect::new(
                    0,
                    0,
                    u32::from(screen.width_in_pixels),
                    u32::from(screen.height_in_pixels),
                ),
                true,
            )]);
        };

        let monitors = randr
            .monitors(&self.conn, self.root)
            .map_err(|err| Error::Platform(format!("RandR monitor query failed: {err}")))?;

        if monitors.is_empty() {
            return Ok(vec![(
                "Screen".to_owned(),
                PixelRect::new(
                    0,
                    0,
                    u32::from(screen.width_in_pixels),
                    u32::from(screen.height_in_pixels),
                ),
                true,
            )]);
        }

        let primary = wire::primary_index(&monitors);

        // Name atoms resolved in one pipelined batch, as with interning.
        let cookies = monitors
            .iter()
            .map(|m| xproto::get_atom_name(&self.conn, m.name).ok())
            .collect::<Vec<_>>();

        Ok(monitors
            .iter()
            .enumerate()
            .zip(cookies)
            .map(|((index, monitor), cookie)| {
                let name = cookie
                    .and_then(|c| c.reply().ok())
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("Monitor {}", index + 1));
                (
                    name,
                    PixelRect::new(monitor.x, monitor.y, monitor.width, monitor.height),
                    primary == Some(index),
                )
            })
            .collect())
    }

    /// The desktop-wide work area, already narrowed to the current desktop.
    fn desktop_work_area(&self) -> Option<PixelRect> {
        let desktop = self
            .property(
                self.root,
                self.atoms.net_current_desktop,
                u32::from(AtomEnum::CARDINAL),
            )
            .and_then(|(_, bytes)| ewmh::parse_u32_list(&bytes).first().copied())
            .unwrap_or(0);

        let (_, bytes) = self.property(
            self.root,
            self.atoms.net_workarea,
            u32::from(AtomEnum::CARDINAL),
        )?;

        ewmh::parse_work_area(&bytes, desktop).map(PixelRect::from)
    }

    fn display_table(&self) -> Result<Vec<(DisplayId, PixelRect, bool)>> {
        Ok(self
            .monitors()?
            .into_iter()
            .enumerate()
            .map(|(index, (name, rect, primary))| {
                (DisplayId(display_id(index, &name)), rect, primary)
            })
            .collect())
    }

    fn window_bounds(&self, window: u32, include_frame: bool) -> Option<PixelRect> {
        let geometry = xproto::get_geometry(&self.conn, window)
            .ok()?
            .reply()
            .ok()?;

        // `GetGeometry` is parent-relative and the parent is usually the window
        // manager's frame, so the raw x/y is an offset inside the decoration,
        // not a desktop position. Translating to the root is what turns it into
        // something a user could point at.
        let origin = xproto::translate_coordinates(&self.conn, window, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;

        let border = i32::from(geometry.border_width);
        let rect = ewmh::WireRect {
            x: i32::from(origin.dst_x) - border,
            y: i32::from(origin.dst_y) - border,
            width: u32::from(geometry.width) + 2 * border.unsigned_abs(),
            height: u32::from(geometry.height) + 2 * border.unsigned_abs(),
        };

        let rect = if include_frame {
            self.property(
                window,
                self.atoms.net_frame_extents,
                u32::from(AtomEnum::CARDINAL),
            )
            .and_then(|(_, bytes)| ewmh::parse_frame_extents(&bytes))
            .map_or(rect, |extents| ewmh::apply_frame_extents(rect, extents))
        } else {
            rect
        };

        Some(PixelRect::from(rect))
    }

    fn window_title(&self, window: u32) -> Option<String> {
        self.property(window, self.atoms.net_wm_name, self.atoms.utf8_string)
            .and_then(|(_, bytes)| ewmh::parse_utf8_name(&bytes))
            .or_else(|| {
                // `WM_NAME` is the pre-EWMH fallback and is Latin-1, so decoding
                // it as UTF-8 mangles every accented character in a title.
                self.property(
                    window,
                    u32::from(AtomEnum::WM_NAME),
                    u32::from(AtomEnum::STRING),
                )
                .and_then(|(_, bytes)| ewmh::parse_latin1_name(&bytes))
            })
    }

    fn window_application(&self, window: u32) -> Option<String> {
        self.property(
            window,
            u32::from(AtomEnum::WM_CLASS),
            u32::from(AtomEnum::STRING),
        )
        .and_then(|(_, bytes)| ewmh::application_name(&bytes))
    }

    fn is_mapped(&self, window: u32) -> bool {
        xproto::get_window_attributes(&self.conn, window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attrs| attrs.map_state == MapState::VIEWABLE)
    }

    /// Finds the window manager's frame for a client window.
    ///
    /// Walks up to the child of the root, which is the reparenting window
    /// manager's decoration. Capturing that rather than the client window is
    /// what makes a window screenshot include its title bar.
    fn frame_window(&self, window: u32) -> u32 {
        let mut current = window;
        for _ in 0..8 {
            let Some(reply) = xproto::query_tree(&self.conn, current)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
            else {
                return current;
            };
            if reply.parent == self.root || reply.parent == 0 {
                return current;
            }
            current = reply.parent;
        }
        current
    }

    /// The visual and pixel format details needed to interpret `GetImage` bytes.
    fn visual_layout(&self, depth: u8, visual_id: u32) -> Result<(pixels::ByteLayout, u8)> {
        let screen = self.screen()?;

        let visual: Option<&Visualtype> = screen
            .allowed_depths
            .iter()
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.visual_id == visual_id);

        let bits_per_pixel = self
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .map_or(32, |format| format.bits_per_pixel);

        let scanline_pad = self
            .conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .map_or(32, |format| format.scanline_pad);

        let visual = visual.ok_or_else(|| Error::Unsupported {
            what: "capturing this X11 visual".into(),
            why: format!("the server reported visual {visual_id:#x}, which is not on this screen"),
        })?;

        let layout = pixels::byte_layout(
            visual.red_mask,
            visual.green_mask,
            visual.blue_mask,
            depth,
            bits_per_pixel,
            self.image_lsb_first,
        )
        .ok_or_else(|| Error::Unsupported {
            what: "capturing this X11 visual".into(),
            why: format!(
                "depth {depth} at {bits_per_pixel} bits per pixel is not a 32-bit true-colour \
                 visual; Scrozz only handles 32-bit RGB visuals, which is every display in \
                 ordinary use"
            ),
        })?;

        Ok((layout, scanline_pad))
    }

    /// Fetches a rectangle of the root window as a [`Frame`].
    fn grab(&self, drawable: u32, region: PixelRect, scale: ScaleFactor) -> Result<Frame> {
        if region.is_empty() {
            return Err(Error::InvalidRequest(
                "the capture region has no area".into(),
            ));
        }

        let width = u16::try_from(region.width).map_err(|_| {
            Error::InvalidRequest("the capture region is wider than X11 permits".into())
        })?;
        let height = u16::try_from(region.height).map_err(|_| {
            Error::InvalidRequest("the capture region is taller than X11 permits".into())
        })?;
        let x = i16::try_from(region.x)
            .map_err(|_| Error::InvalidRequest("the capture region is off-screen".into()))?;
        let y = i16::try_from(region.y)
            .map_err(|_| Error::InvalidRequest("the capture region is off-screen".into()))?;

        let reply = xproto::get_image(
            &self.conn,
            ImageFormat::Z_PIXMAP,
            drawable,
            x,
            y,
            width,
            height,
            pixels::all_planes(),
        )
        .map_err(platform)?
        .reply()
        .map_err(|err| {
            // `BadDrawable`/`BadMatch` here almost always means the window was
            // closed or unmapped between enumeration and capture, which the
            // contract calls TargetGone rather than a platform fault.
            Error::TargetGone(format!(
                "the X server refused to read those pixels, which usually means the window \
                 closed or was minimised during capture: {err}"
            ))
        })?;

        let (layout, scanline_pad) = self.visual_layout(reply.depth, reply.visual)?;
        let bits_per_pixel = u8::try_from(layout.bytes_per_pixel * 8).unwrap_or(32);
        let src_stride = pixels::scanline_stride(region.width, bits_per_pixel, scanline_pad)
            .ok_or_else(|| Error::Platform("the capture is too large to address".into()))?;

        let (data, format) = pixels::repack(
            &reply.data,
            src_stride,
            region.width,
            region.height,
            &layout,
        );

        let frame = Frame {
            data,
            size: PhysicalSize::new(f64::from(region.width), f64::from(region.height)),
            stride: region.width as usize * 4,
            format,
            // X11 has no colour management in the protocol. `_ICC_PROFILE` on
            // the root window sometimes carries one, but claiming sRGB when the
            // monitor is wide-gamut produces silently wrong colour, so the
            // honest answer is that this is unknown.
            color_space: ColorSpace::Unknown,
            scale,
        };

        debug_assert!(frame.is_well_formed(), "repack produced a short buffer");
        Ok(frame)
    }

    fn display_by_id(&self, id: &DisplayId) -> Result<Display> {
        self.displays()?
            .into_iter()
            .find(|display| &display.id == id)
            .ok_or_else(|| Error::TargetGone(format!("display {} is no longer connected", id.0)))
    }

    fn window_handle(&self, id: &WindowId) -> Result<u32> {
        id.0.strip_prefix("x11:")
            .and_then(|raw| u32::from_str_radix(raw, 16).ok())
            .ok_or_else(|| Error::TargetGone(format!("{} is not an X11 window handle", id.0)))
    }
}

impl TargetEnumerator for X11Backend {
    fn displays(&self) -> Result<Vec<Display>> {
        let work_area = self.desktop_work_area();

        Ok(self
            .monitors()?
            .into_iter()
            .enumerate()
            .map(|(index, (name, bounds, is_primary))| {
                layout::to_display(
                    DisplayId(display_id(index, &name)),
                    name,
                    bounds,
                    layout::work_area_for(bounds, work_area),
                    self.scale,
                    is_primary,
                )
            })
            .collect())
    }

    fn windows(&self) -> Result<Vec<Window>> {
        // Stacking order is what the contract wants; `_NET_CLIENT_LIST` is in
        // age order and would put the oldest window first, which is never what
        // a picker should offer.
        let stacking = self
            .property(
                self.root,
                self.atoms.net_client_list_stacking,
                u32::from(AtomEnum::WINDOW),
            )
            .map(|(_, bytes)| ewmh::stacking_to_front_first(ewmh::parse_u32_list(&bytes)));

        let handles = match stacking {
            Some(list) if !list.is_empty() => list,
            _ => self
                .property(
                    self.root,
                    self.atoms.net_client_list,
                    u32::from(AtomEnum::WINDOW),
                )
                .map(|(_, bytes)| ewmh::parse_u32_list(&bytes))
                .ok_or_else(|| Error::Unsupported {
                    what: "listing windows".into(),
                    why: "this X11 session has no EWMH-compliant window manager, so no window \
                          list is published; capture a display or a region instead"
                        .into(),
                })?,
        };

        let displays = self.display_table()?;

        Ok(handles
            .into_iter()
            .filter_map(|handle| {
                let mapped = self.is_mapped(handle);
                let state = self
                    .property(handle, self.atoms.wm_state, 0)
                    .and_then(|(_, bytes)| ewmh::parse_wm_state(&bytes));
                if !ewmh::is_listable(state, mapped) {
                    return None;
                }

                let bounds = self.window_bounds(handle, true)?;
                let display = layout::display_for_window(bounds, &displays)?;

                Some(Window {
                    id: WindowId(format!("x11:{handle:08x}")),
                    title: self.window_title(handle),
                    application: self.window_application(handle),
                    bounds: bounds.to_logical(self.scale.get()),
                    display,
                    is_visible: mapped,
                })
            })
            .collect())
    }

    fn active_display(&self) -> Result<Display> {
        let pointer = xproto::query_pointer(&self.conn, self.root)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;

        let displays = self.display_table()?;
        let id = layout::display_containing(
            i32::from(pointer.root_x),
            i32::from(pointer.root_y),
            &displays,
        )
        .ok_or_else(|| Error::Platform("no displays are connected".into()))?;

        self.display_by_id(&id)
    }
}

impl CaptureBackend for X11Backend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        if request.cursor == scrozz_core::CursorMode::Visible {
            // Compositing the pointer needs XFIXES for the cursor image, which
            // is another feature-gated extension. Failing here would make the
            // whole capture unavailable over an optional decoration, so the
            // capture proceeds and the omission is logged.
            tracing::warn!(
                "cursor capture needs the XFIXES extension, which is not compiled in; \
                 capturing without the pointer"
            );
        }

        let frame = match &request.target {
            CaptureTarget::Display(id) => {
                let display = self.display_by_id(id)?;
                let region = physical_rect(display.bounds, display.scale);
                self.grab(self.root, region, display.scale)?
            }

            CaptureTarget::AllDisplays => {
                let rects: Vec<PixelRect> = self
                    .monitors()?
                    .into_iter()
                    .map(|(_, rect, _)| rect)
                    .collect();
                let region = layout::bounding_box(&rects)
                    .ok_or_else(|| Error::Platform("no displays are connected".into()))?;
                self.grab(self.root, region, self.scale)?
            }

            CaptureTarget::Region(rect) => {
                let screen = self.screen()?;
                let root_rect = PixelRect::new(
                    0,
                    0,
                    u32::from(screen.width_in_pixels),
                    u32::from(screen.height_in_pixels),
                );
                let region = layout::region_to_pixels(*rect, self.scale.get(), root_rect)
                    .ok_or_else(|| {
                        Error::InvalidRequest("the selected region lies entirely off-screen".into())
                    })?;
                self.grab(self.root, region, self.scale)?
            }

            CaptureTarget::Window(id) => {
                let client = self.window_handle(id)?;
                let drawable = if request.include_window_shadow {
                    self.frame_window(client)
                } else {
                    client
                };
                let bounds = self
                    .window_bounds(drawable, false)
                    .ok_or_else(|| Error::TargetGone(format!("window {} has closed", id.0)))?;

                // Reading from the window drawable rather than the root is what
                // excludes anything stacked on top of it — but only when a
                // compositing manager is redirecting the window's contents.
                // Without one, X has no stored pixels for obscured areas and
                // the server returns whatever is on screen there.
                self.grab(
                    drawable,
                    PixelRect::new(0, 0, bounds.width, bounds.height),
                    self.scale,
                )?
            }
        };

        Ok(Capture {
            frame,
            provenance: match &request.target {
                CaptureTarget::Display(_) => Provenance::Display,
                CaptureTarget::Window(_) => Provenance::Window,
                CaptureTarget::Region(_) => Provenance::Region,
                CaptureTarget::AllDisplays => Provenance::AllDisplays,
            },
            target: request.target.clone(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// A stable-for-a-session display identifier.
///
/// The RandR output name is used when there is one, because it survives a
/// monitor being re-detected in a way an index does not; the index is only a
/// disambiguator for the duplicate names cloned outputs produce.
fn display_id(index: usize, name: &str) -> String {
    format!("x11:{index}:{name}")
}

fn physical_rect(bounds: LogicalRect, scale: ScaleFactor) -> PixelRect {
    let physical = bounds.to_physical(scale);
    PixelRect::new(
        physical.origin.x as i32,
        physical.origin.y as i32,
        physical.size.width.max(0.0) as u32,
        physical.size.height.max(0.0) as u32,
    )
}

fn read_scale(conn: &RustConnection, root: u32) -> ScaleFactor {
    let resources = xproto::get_property(
        conn,
        false,
        root,
        u32::from(AtomEnum::RESOURCE_MANAGER),
        u32::from(AtomEnum::STRING),
        0,
        65_536,
    )
    .ok()
    .and_then(|cookie| cookie.reply().ok())
    .map(|reply| String::from_utf8_lossy(&reply.value).into_owned());

    let gdk = std::env::var("GDK_SCALE").ok();
    let qt = std::env::var("QT_SCALE_FACTOR").ok();

    scale::resolve_scale(gdk.as_deref(), qt.as_deref(), resources.as_deref())
}

fn platform<E: std::fmt::Display>(err: E) -> Error {
    Error::Platform(format!("X11 request failed: {err}"))
}
