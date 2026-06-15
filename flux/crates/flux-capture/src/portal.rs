//! Real `xdg-desktop-portal` session negotiation via [`ashpd`].
//!
//! [`XdgPortalSession`] implements [`PortalSession`] by driving the
//! `org.freedesktop.portal.ScreenCast` (and, when input is requested,
//! `org.freedesktop.portal.RemoteDesktop`) D-Bus interfaces over `zbus`.
//!
//! The negotiation follows the portal handshake:
//! `CreateSession` → `SelectSources` (+ `SelectDevices` for input) → `Start`
//! (which raises the compositor's consent dialog) → `OpenPipeWireRemote`
//! (which yields the authorized PipeWire fd). The resulting node ids and fd are
//! handed to the synchronous [`PipewireFrameSource`](crate::session::PipewireFrameSource)
//! that actually pulls frames.
//!
//! All ashpd proxies share a process-wide `zbus` session connection, so the
//! `Screencast`/`RemoteDesktop`/`Session` handles are `'static` and can be
//! owned by this struct for the lifetime of the capture. The PipeWire fd is
//! held as an [`OwnedFd`] here; [`PortalGrant::pipewire_fd`] is a borrowed
//! [`RawFd`] view that the frame source `dup`s before use, so this session
//! must outlive the frame source.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode as XdpCursorMode, Screencast, SourceType, Stream};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use ashpd::WindowIdentifier;
use async_trait::async_trait;
use flux_core::error::{FluxError, Result};

use crate::session::{CursorMode, PortalGrant, PortalOptions, PortalSession, PortalStream, SourceKind};

/// A live `xdg-desktop-portal` capture (+ optional input) session.
///
/// Construct with [`XdgPortalSession::new`] and call
/// [`PortalSession::negotiate`] to prompt the user and obtain the granted
/// streams + PipeWire fd.
#[derive(Default)]
pub struct XdgPortalSession {
    streams: Vec<PortalStream>,
    restore_token: Option<String>,
    // Proxies / session handles kept alive for the session's duration. They
    // share the process-wide zbus connection (hence `'static`).
    screencast: Option<Screencast<'static>>,
    remote_desktop: Option<RemoteDesktop<'static>>,
    sc_session: Option<Session<'static, Screencast<'static>>>,
    rd_session: Option<Session<'static, RemoteDesktop<'static>>>,
    /// Owned PipeWire fd from `OpenPipeWireRemote`; `PortalGrant` borrows it.
    pipewire_fd: Option<OwnedFd>,
}

impl XdgPortalSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Negotiate a combined ScreenCast + RemoteDesktop session (capture +
    /// input) on a single `RemoteDesktop` session, so one consent dialog
    /// covers both.
    async fn negotiate_with_input(&mut self, opts: &PortalOptions) -> Result<PortalGrant> {
        let remote_desktop = RemoteDesktop::new().await.map_err(portal_err)?;
        let screencast = Screencast::new().await.map_err(portal_err)?;
        let session = remote_desktop.create_session().await.map_err(portal_err)?;

        remote_desktop
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                opts.restore_token.as_deref(),
                persist_mode(opts),
            )
            .await
            .map_err(portal_err)?;

        screencast
            .select_sources(
                &session,
                to_xdp_cursor_mode(opts.cursor_mode),
                to_source_types(&opts.source_kinds),
                opts.multiple,
                opts.restore_token.as_deref(),
                persist_mode(opts),
            )
            .await
            .map_err(portal_err)?;

        let response = remote_desktop
            .start(&session, &WindowIdentifier::default())
            .await
            .map_err(portal_err)?
            .response()
            .map_err(portal_err)?;

        let xdp_streams = response
            .streams()
            .ok_or_else(|| FluxError::Capture("portal granted no screen-cast streams".into()))?;
        let streams = map_streams(xdp_streams, &opts.source_kinds);
        if streams.is_empty() {
            return Err(FluxError::Capture("portal granted an empty stream set".into()));
        }

        let fd: OwnedFd = screencast.open_pipe_wire_remote(&session).await.map_err(portal_err)?;
        let raw_fd = fd.as_raw_fd();
        let restore_token = response.restore_token().map(str::to_owned);

        self.screencast = Some(screencast);
        self.remote_desktop = Some(remote_desktop);
        self.rd_session = Some(session);
        self.pipewire_fd = Some(fd);
        self.streams = streams.clone();
        self.restore_token = restore_token.clone();

        Ok(PortalGrant {
            streams,
            pipewire_fd: raw_fd,
            restore_token,
            remote_desktop: true,
        })
    }

    /// Negotiate a ScreenCast-only session (capture, no input).
    async fn negotiate_capture_only(&mut self, opts: &PortalOptions) -> Result<PortalGrant> {
        let screencast = Screencast::new().await.map_err(portal_err)?;
        let session = screencast.create_session().await.map_err(portal_err)?;

        screencast
            .select_sources(
                &session,
                to_xdp_cursor_mode(opts.cursor_mode),
                to_source_types(&opts.source_kinds),
                opts.multiple,
                opts.restore_token.as_deref(),
                persist_mode(opts),
            )
            .await
            .map_err(portal_err)?;

        let response = screencast
            .start(&session, &WindowIdentifier::default())
            .await
            .map_err(portal_err)?
            .response()
            .map_err(portal_err)?;

        let streams = map_streams(response.streams(), &opts.source_kinds);
        if streams.is_empty() {
            return Err(FluxError::Capture("portal granted an empty stream set".into()));
        }

        let fd: OwnedFd = screencast.open_pipe_wire_remote(&session).await.map_err(portal_err)?;
        let raw_fd = fd.as_raw_fd();
        let restore_token = response.restore_token().map(str::to_owned);

        self.screencast = Some(screencast);
        self.sc_session = Some(session);
        self.pipewire_fd = Some(fd);
        self.streams = streams.clone();
        self.restore_token = restore_token.clone();

        Ok(PortalGrant {
            streams,
            pipewire_fd: raw_fd,
            restore_token,
            remote_desktop: false,
        })
    }

    /// The PipeWire fd from the last successful negotiation, if any.
    pub fn pipewire_fd(&self) -> Option<RawFd> {
        self.pipewire_fd.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// The restore token offered by the last successful negotiation, if any.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }
}

#[async_trait]
impl PortalSession for XdgPortalSession {
    async fn negotiate(&mut self, opts: PortalOptions) -> Result<PortalGrant> {
        if opts.with_remote_desktop {
            self.negotiate_with_input(&opts).await
        } else {
            self.negotiate_capture_only(&opts).await
        }
    }

    fn streams(&self) -> &[PortalStream] {
        &self.streams
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(session) = self.rd_session.take() {
            let _ = session.close().await;
        }
        if let Some(session) = self.sc_session.take() {
            let _ = session.close().await;
        }
        self.screencast = None;
        self.remote_desktop = None;
        self.pipewire_fd = None;
        self.streams.clear();
        Ok(())
    }
}

fn portal_err(e: ashpd::Error) -> FluxError {
    FluxError::Capture(format!("xdg-desktop-portal: {e}"))
}

fn persist_mode(_opts: &PortalOptions) -> PersistMode {
    // Request a restore token so a later reconnection can skip the consent
    // prompt; the grant stays valid until the user explicitly revokes it.
    PersistMode::ExplicitlyRevoked
}

fn to_xdp_cursor_mode(mode: CursorMode) -> XdpCursorMode {
    match mode {
        CursorMode::Hidden => XdpCursorMode::Hidden,
        CursorMode::Embedded => XdpCursorMode::Embedded,
        CursorMode::Metadata => XdpCursorMode::Metadata,
    }
}

fn to_source_types(kinds: &[SourceKind]) -> BitFlags<SourceType> {
    let mut types = BitFlags::<SourceType>::empty();
    for kind in kinds {
        types |= match kind {
            SourceKind::Monitor => SourceType::Monitor,
            SourceKind::Window => SourceType::Window,
            SourceKind::Virtual => SourceType::Virtual,
        };
    }
    if types.is_empty() {
        types |= SourceType::Monitor;
    }
    types
}

fn from_source_type(ty: Option<SourceType>, fallback: SourceKind) -> SourceKind {
    match ty {
        Some(SourceType::Monitor) => SourceKind::Monitor,
        Some(SourceType::Window) => SourceKind::Window,
        Some(SourceType::Virtual) => SourceKind::Virtual,
        _ => fallback,
    }
}

fn map_streams(streams: &[Stream], requested: &[SourceKind]) -> Vec<PortalStream> {
    let fallback = requested.first().copied().unwrap_or(SourceKind::Monitor);
    streams
        .iter()
        .map(|s| PortalStream {
            node_id: s.pipe_wire_node_id(),
            position: s.position(),
            size: s.size().map(|(w, h)| (w as u32, h as u32)),
            kind: from_source_type(s.source_type(), fallback),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kinds_map_to_xdp_flags() {
        let flags = to_source_types(&[SourceKind::Monitor, SourceKind::Window]);
        assert!(flags.contains(SourceType::Monitor));
        assert!(flags.contains(SourceType::Window));
        assert!(!flags.contains(SourceType::Virtual));
    }

    #[test]
    fn empty_source_kinds_default_to_monitor() {
        let flags = to_source_types(&[]);
        assert!(flags.contains(SourceType::Monitor));
        assert!(!flags.contains(SourceType::Window));
    }

    #[test]
    fn cursor_modes_round_trip() {
        assert!(matches!(to_xdp_cursor_mode(CursorMode::Hidden), XdpCursorMode::Hidden));
        assert!(matches!(
            to_xdp_cursor_mode(CursorMode::Embedded),
            XdpCursorMode::Embedded
        ));
        assert!(matches!(
            to_xdp_cursor_mode(CursorMode::Metadata),
            XdpCursorMode::Metadata
        ));
    }

    #[test]
    fn source_type_falls_back_when_unknown() {
        assert_eq!(
            from_source_type(Some(SourceType::Window), SourceKind::Monitor),
            SourceKind::Window
        );
        assert_eq!(from_source_type(None, SourceKind::Virtual), SourceKind::Virtual);
    }
}
