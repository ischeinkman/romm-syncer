//! The home tab of the UI, used for installing & uninstalling the daemon and manually triggering resyncs.

use std::{io, sync::Arc, time::Duration};

use anyhow::Context;
use buoyant::{
    layout::Layout,
    render::EmbeddedGraphicsView,
    view::{HStack, LayoutExtensions, RenderExtensions, Spacer, Text, VStack},
};
use embedded_graphics::{pixelcolor::Rgb888, prelude::RgbColor};
use embedded_vintage_fonts::FONT_24X32;
use futures::future;
use syncer_model::{
    commands::{DaemonCommand, DaemonCommandBody},
    config::{Config, ParseableDuration},
};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tracing::{info, warn};

use crate::{ApplicationState, ViewState, utils::BackgroundTask};
use crate::{
    components::{button, labeled_checkbox},
    daemon::{daemon_is_running, start_daemon, stop_daemon},
};
use crate::{
    daemon::{daemon_is_installed, install_daemon, reinstall_daemon, uninstall_daemon},
    utils::QuickReadSlot,
};

const REFRESH_STATE_INTERVAL: Duration = Duration::from_millis(200);
const POLL_TIME_OPTIONS: &[Duration] = &[
    Duration::from_secs(60),
    Duration::from_secs(60 * 5),
    Duration::from_secs(60 * 10),
    Duration::from_secs(60 * 15),
    Duration::from_secs(60 * 30),
    Duration::from_secs(60 * 60),
    Duration::from_secs(60 * 60 * 2),
    Duration::from_secs(60 * 60 * 4),
    Duration::from_secs(60 * 60 * 8),
    Duration::MAX, // Disabled
];

const fn cur_poll_idx(duration: Duration) -> usize {
    let mut retvl = 0;
    while POLL_TIME_OPTIONS[retvl].as_secs() < duration.as_secs() {
        retvl += 1;
    }
    if retvl >= POLL_TIME_OPTIONS.len() {
        retvl = POLL_TIME_OPTIONS.len();
    }
    retvl
}

pub struct HomepageState {
    pressed: bool,
    selection: HomePageSelection,
    pub cfg: ApplicationState,
    external_state: Arc<QuickReadSlot<ExternalState>>,
    _external_state_poller: BackgroundTask,
    redraw_trigger: broadcast::Sender<()>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
enum HomePageSelection {
    #[default]
    Nothing,
    DaemonInstalledBox,
    DaemonRunningBox,
    PollTimeSelection,
    FsnotifyBox,
    ReinstallDaemon,
    UninstallDaemon,
    ForceSyncButton,
}

impl HomePageSelection {
    const fn up(&self) -> HomePageSelection {
        use HomePageSelection::*;
        match *self {
            UninstallDaemon | ReinstallDaemon | ForceSyncButton => FsnotifyBox,
            FsnotifyBox => PollTimeSelection,
            PollTimeSelection => DaemonRunningBox,
            DaemonRunningBox => DaemonInstalledBox,
            DaemonInstalledBox | Nothing => Nothing,
        }
    }
    const fn down(&self) -> HomePageSelection {
        use HomePageSelection::*;
        match *self {
            Nothing => DaemonInstalledBox,
            DaemonInstalledBox => DaemonRunningBox,
            DaemonRunningBox => PollTimeSelection,
            PollTimeSelection => FsnotifyBox,
            FsnotifyBox => ReinstallDaemon,
            ReinstallDaemon | UninstallDaemon | ForceSyncButton => Nothing,
        }
    }
    const fn left(&self) -> HomePageSelection {
        use HomePageSelection::*;
        match *self {
            ReinstallDaemon => ForceSyncButton,
            ForceSyncButton => UninstallDaemon,
            UninstallDaemon => ReinstallDaemon,
            other => other,
        }
    }
    const fn right(&self) -> HomePageSelection {
        use HomePageSelection::*;
        match *self {
            ReinstallDaemon => UninstallDaemon,
            UninstallDaemon => ForceSyncButton,
            ForceSyncButton => ReinstallDaemon,
            other => other,
        }
    }
}

impl HomepageState {
    pub async fn new(cfg: ApplicationState) -> Result<Self, anyhow::Error> {
        let external_state = Arc::new(QuickReadSlot::new(ExternalState::new(cfg.clone()).await?));
        let (redraw_trigger, _) = broadcast::channel(5);
        let trigger2 = redraw_trigger.clone();
        let es = Arc::clone(&external_state);
        let _external_state_poller = BackgroundTask::new(async move |flag| {
            while !flag.should_stop() {
                match es.modify_with(async |state| state.reload().await).await {
                    Ok(true) => {
                        trigger2.send(()).ok();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!("Error updating homepage state: {e:?}");
                    }
                }
                tokio::time::sleep(REFRESH_STATE_INTERVAL).await;
            }
        });
        let mut retvl = Self {
            cfg: cfg.clone(),
            pressed: false,
            selection: HomePageSelection::default(),
            external_state,
            _external_state_poller,
            redraw_trigger,
        };
        retvl.reload().await?;
        Ok(retvl)
    }
    async fn reload(&mut self) -> Result<(), anyhow::Error> {
        let needs_redraw = self
            .external_state
            .modify_with(async |state| state.reload().await)
            .await?;
        if needs_redraw {
            self.redraw_trigger.send(()).ok();
        }
        Ok(())
    }
}

impl ViewState for HomepageState {
    async fn up(&mut self) -> Result<(), anyhow::Error> {
        self.selection = self.selection.up();
        Ok(())
    }
    async fn down(&mut self) -> Result<(), anyhow::Error> {
        self.selection = self.selection.down();
        Ok(())
    }
    async fn left(&mut self) -> Result<(), anyhow::Error> {
        if self.selection == HomePageSelection::PollTimeSelection {
            let cur_poll_idx = cur_poll_idx(*self.cfg.config().await.system.poll_interval);
            let prev_poll_idx = cur_poll_idx.saturating_sub(1);
            self.cfg
                .modify_and_save_cfg(move |cfg: &mut Config| {
                    cfg.system.poll_interval = POLL_TIME_OPTIONS[prev_poll_idx].into();
                    future::ready(())
                })
                .await?;
            self.reload().await?;
        } else {
            self.selection = self.selection.left();
        }
        Ok(())
    }
    async fn right(&mut self) -> Result<(), anyhow::Error> {
        if self.selection == HomePageSelection::PollTimeSelection {
            let cur_poll_idx = cur_poll_idx(*self.cfg.config().await.system.poll_interval);
            let next_poll_idx = (POLL_TIME_OPTIONS.len() - 1).min(cur_poll_idx + 1);
            self.cfg
                .modify_and_save_cfg(move |cfg: &mut Config| {
                    cfg.system.poll_interval = POLL_TIME_OPTIONS[next_poll_idx].into();
                    future::ready(())
                })
                .await?;
            self.reload().await?;
        } else {
            self.selection = self.selection.right();
        }
        Ok(())
    }
    async fn press(&mut self) -> Result<(), anyhow::Error> {
        self.pressed = true;
        Ok(())
    }
    async fn release(&mut self) -> Result<(), anyhow::Error> {
        use HomePageSelection::*;
        let external_state = self.external_state.read().clone();
        match self.selection {
            ReinstallDaemon => {
                reinstall_daemon().await?;
                self.reload().await?;
            }
            UninstallDaemon => {
                uninstall_daemon().await?;
                self.reload().await?;
            }
            ForceSyncButton => {
                let res = self
                    .cfg
                    .socket
                    .send(&DaemonCommand::new(DaemonCommandBody::DoSync))
                    .await;
                match res {
                    Ok(()) => {
                        self.reload().await?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        info!("User tried to resync the daemon when it isn't running.");
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
            DaemonInstalledBox if external_state.daemon_installed => {
                uninstall_daemon().await?;
                self.reload().await?;
            }
            DaemonInstalledBox => {
                install_daemon().await?;
                self.reload().await?;
            }
            DaemonRunningBox if external_state.daemon_running => {
                stop_daemon().await?;
                self.reload().await?;
            }
            DaemonRunningBox => {
                start_daemon().await?;
                self.reload().await?;
            }
            FsnotifyBox => {
                self.cfg
                    .modify_and_save_cfg(|cfg: &mut Config| {
                        cfg.system.sync_on_file_change = !external_state.fs_notify_enabled;
                        future::ready(())
                    })
                    .await?;
                self.reload().await?;
            }
            PollTimeSelection | Nothing => {}
        }
        self.pressed = false;
        Ok(())
    }
    fn build_view(&self) -> impl EmbeddedGraphicsView<Rgb888> + Layout + '_ {
        let state = self.external_state.read();
        build_view(
            self.selection,
            self.pressed,
            state.daemon_installed,
            state.daemon_running,
            state.poll_interval,
            state.fs_notify_enabled,
        )
    }
    async fn trigger_redraw(&mut self) -> Result<(), anyhow::Error> {
        let mut rcv = self.redraw_trigger.subscribe();
        rcv.recv().await.ok();
        while !matches!(
            rcv.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Closed)
        ) {}
        Ok(())
    }
}

fn labelled_scrollable_options<'a>(
    label: impl AsRef<str> + Clone + 'a,
    current_option: impl AsRef<str> + Clone + 'a,
    is_selected: bool,
) -> impl EmbeddedGraphicsView<Rgb888> + Layout + 'a {
    const LABEL_COLOR: Rgb888 = Rgb888::BLACK;
    const LABEL_SELECTED_COLOR: Rgb888 = Rgb888::BLUE;
    const ARROW_COLOR: Rgb888 = Rgb888::BLACK;
    const ARROW_SELECTED_COLOR: Rgb888 = Rgb888::BLUE;
    const OPTION_COLOR: Rgb888 = Rgb888::BLACK;
    const OPTION_SELECTED_COLOR: Rgb888 = Rgb888::BLUE;

    let label_color: Rgb888 = if is_selected {
        LABEL_SELECTED_COLOR
    } else {
        LABEL_COLOR
    };
    let label = Text::new(label, &FONT_24X32).foreground_color(label_color);

    let arrow_color: Rgb888 = if is_selected {
        ARROW_SELECTED_COLOR
    } else {
        ARROW_COLOR
    };
    let option_color: Rgb888 = if is_selected {
        OPTION_SELECTED_COLOR
    } else {
        OPTION_COLOR
    };

    let scrollable = HStack::new((
        Text::new("<", &FONT_24X32).foreground_color(arrow_color),
        Text::new(current_option, &FONT_24X32).foreground_color(option_color),
        Text::new(">", &FONT_24X32).foreground_color(arrow_color),
    ))
    .flex_frame();

    HStack::new((label, Spacer::default(), scrollable))
        .flex_frame()
        .with_infinite_max_width()
}

fn build_view(
    selection: HomePageSelection,
    pressed: bool,
    daemon_installed: bool,
    daemon_running: bool,
    poll_interval: ParseableDuration,
    fs_notify_enabled: bool,
) -> impl EmbeddedGraphicsView<Rgb888> + Layout {
    let installed_box = labeled_checkbox(
        "Daemon installed",
        selection == HomePageSelection::DaemonInstalledBox,
        daemon_installed,
    );
    let running_box = labeled_checkbox(
        "Daemon Running",
        selection == HomePageSelection::DaemonRunningBox,
        daemon_running,
    );
    let uninstall_btn = button(
        "Uninstall",
        selection == HomePageSelection::UninstallDaemon,
        selection == HomePageSelection::UninstallDaemon && pressed,
    );
    let reinstall_btn = button(
        "Reinstall",
        selection == HomePageSelection::ReinstallDaemon,
        selection == HomePageSelection::ReinstallDaemon && pressed,
    );

    let sync_btn = button(
        "Force Sync",
        selection == HomePageSelection::ForceSyncButton,
        selection == HomePageSelection::ForceSyncButton && pressed,
    );

    let current_poll_time = poll_interval.to_string();
    let poll_time_cfg = labelled_scrollable_options(
        "Poll time",
        current_poll_time,
        selection == HomePageSelection::PollTimeSelection,
    );

    let fs_notify_box = labeled_checkbox(
        "Sync on change",
        selection == HomePageSelection::FsnotifyBox,
        fs_notify_enabled,
    );

    let btns = HStack::new((reinstall_btn, uninstall_btn, sync_btn));
    VStack::new((
        installed_box,
        running_box,
        poll_time_cfg,
        fs_notify_box,
        btns,
    ))
    .frame()
}

#[derive(Clone)]
struct ExternalState {
    daemon_installed: bool,
    daemon_running: bool,
    fs_notify_enabled: bool,
    app_state: ApplicationState,
    poll_interval: ParseableDuration,
}

impl ExternalState {
    pub async fn new(app_state: ApplicationState) -> Result<Self, anyhow::Error> {
        let mut retvl = Self {
            daemon_installed: false,
            daemon_running: false,
            fs_notify_enabled: false,
            app_state,
            poll_interval: ParseableDuration::new(Duration::default()),
        };
        retvl.reload().await?;
        Ok(retvl)
    }
    pub async fn reload(&mut self) -> Result<bool, anyhow::Error> {
        let mut modified = false;

        macro_rules! modify {
            ($slot:expr, $value:expr) => {{
                let value = $value;
                if $slot != value {
                    $slot = value;
                    modified = true;
                }
            }};
        }

        modify!(
            self.daemon_installed,
            daemon_is_installed()
                .await
                .context("Error checking daemon install state")?
        );
        modify!(
            self.daemon_running,
            daemon_is_running()
                .await
                .context("Error checking daemon run state")?
        );
        let cfg = self.app_state.config().await;
        modify!(self.poll_interval, cfg.system.poll_interval);
        modify!(self.fs_notify_enabled, cfg.system.sync_on_file_change);
        Ok(modified)
    }
}
