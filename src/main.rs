use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;
use std::error::Error;

use wry::application::dpi::LogicalSize;
use wry::application::event::{Event, WindowEvent};
use wry::application::event_loop::{ControlFlow, EventLoop};
use wry::application::window::{WindowBuilder, WindowId};
use wry::webview::WebViewBuilder;

#[cfg(target_os = "macos")]
use wry::application::platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS};

mod cfg;
mod input;
mod ipc;
mod keys;
mod panic;
mod resources;
mod scrn;
mod state;
mod util;
mod webserver;

use crate::state::State;
use cfg::Config;
use ds::DsMode;
use ipc::*;
use webserver::SetAddr;

const PERCENT_WIDTH: f64 = 0.7906295754026355;
const PERCENT_HEIGHT: f64 = 0.42;

struct Windows {
    main: WindowId,
    #[cfg(target_os = "linux")]
    stdout: WindowId,
}

fn create_macos_window(event_loop: &EventLoop<()>, width: u32, height: u32) -> wry::Result<(WindowId, Box<dyn std::any::Any>)> {
    #[cfg(target_os = "macos")]
    {
        let window = WindowBuilder::new()
            .with_title("Conductor DS")
            .with_inner_size(LogicalSize::new(
                width as f64 * PERCENT_WIDTH,
                height as f64 * PERCENT_HEIGHT,
            ))
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .build(event_loop)?;

        let id = window.id();

        let webview = WebViewBuilder::new(window)?
            .with_transparent(true)
            .with_url("http://localhost:0")?  // Port will be set later
            .build()?;

        Ok((id, Box::new(webview)))
    }

    #[cfg(not(target_os = "macos"))]
    Err(wry::Error::InitializationError)
}

fn create_windows(
    event_loop: &EventLoop<()>, 
    port: u16, 
    width: u32, 
    height: u32
) -> wry::Result<(Windows, Vec<Box<dyn std::any::Any>>)> {
    let mut webviews = Vec::new();
    
    #[cfg(target_os = "macos")]
    let (main_id, main_webview) = create_macos_window(event_loop, width, height)?;
    
    #[cfg(not(target_os = "macos"))]
    let (main_id, main_webview) = {
        let window = WindowBuilder::new()
            .with_title("Conductor DS")
            .with_inner_size(LogicalSize::new(
                width as f64 * PERCENT_WIDTH,
                height as f64 * PERCENT_HEIGHT,
            ))
            .build(event_loop)?;
        
        let id = window.id();
        
        let webview = WebViewBuilder::new(window)?
            .with_url(&format!("http://localhost:{}", port))?
            .with_initialization_script(&format!("window.startapp({});", port))
            .build()?;
        
        (id, Box::new(webview))
    };
    
    webviews.push(main_webview);

    #[cfg(target_os = "linux")]
    let (stdout_id, stdout_webview) = {
        let stdout_window = WindowBuilder::new()
            .with_title("Robot Console")
            .with_inner_size(LogicalSize::new(650.0, 650.0))
            .build(event_loop)?;
        
        let id = stdout_window.id();
        
        let webview = WebViewBuilder::new(stdout_window)?
            .with_url(&format!("http://localhost:{}/stdout", port))?
            .with_initialization_script(&format!("window.startapp({});", port))
            .build()?;
        
        (id, Box::new(webview))
    };

    #[cfg(target_os = "linux")]
    webviews.push(stdout_webview);

    Ok((
        Windows {
            main: main_id,
            #[cfg(target_os = "linux")]
            stdout: stdout_id,
        },
        webviews,
    ))
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    #[cfg(target_os = "windows")]
    {
        use tinyfiledialogs::{message_box_ok, MessageBoxIcon};
        message_box_ok(
            "Unsupported Environment",
            "The Conductor Driver Station is not supported on Windows. Please use the NI Driver Station instead.",
            MessageBoxIcon::Error,
        );
        std::process::exit(1);
    }

    let state = Arc::new(RwLock::new(State::new()));
    let end_state = state.clone();
    let (tx, rx) = mpsc::channel();
    let (stdout_tx, stdout_rx) = mpsc::channel();

    let mut cfg = confy::load::<Config>("conductor").map_err(|e| format!("Failed to load config: {}", e))?;

    if std::env::var("RUST_BACKTRACE").is_err() {
        std::panic::set_hook(Box::new(panic::hook));
    }

    let port = webserver::launch_webserver(state.clone(), tx, stdout_tx);
    log::info!("Webserver launched on port {}", port);

    let (width, height) = scrn::screen_resolution();
    log::info!("Detected Resolution {} {}", width, height);

    let event_loop = EventLoop::new();
    let (windows, webviews) = create_windows(&event_loop, port, width, height)?;

    let addr = rx.recv().map_err(|e| format!("Failed to receive address: {}", e))?;

    #[cfg(target_os = "linux")]
    {
        let stdout_addr = stdout_rx.recv().map_err(|e| format!("Failed to receive stdout address: {}", e))?;
        addr.do_send(SetAddr { addr: stdout_addr });
    }

    if let Ok(mut state) = state.write() {
        state.wire_stdout(addr.clone());

        if cfg.team_number != 0 {
            addr.do_send(Message::UpdateTeamNumber {
                team_number: cfg.team_number,
                from_backend: true,
            });
            state.update_ds(cfg.team_number);
        }
    }

    let keybindings_enabled = keys::bind_keys(state.clone(), addr.clone());
    addr.do_send(Message::Capabilities {
        backend_keybinds: keybindings_enabled,
    });

    input::input_thread(addr.clone());

    {
        let state = state.clone();
        let addr = addr.clone();
        thread::spawn(move || {
            loop {
                if let Ok(state) = state.read() {
                    let ds = &state.ds;
                    let msg = Message::RobotStateUpdate {
                        comms_alive: ds.trace().is_connected(),
                        code_alive: ds.trace().is_code_started(),
                        simulator: ds.ds_mode() == DsMode::Simulation,
                        joysticks: input::JS_STATE
                            .get()
                            .and_then(|js| js.read().ok())
                            .map(|js| js.has_joysticks())
                            .unwrap_or(false),
                        voltage: ds.battery_voltage(),
                    };
                    addr.do_send(msg);
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
    }

    let _webviews = webviews;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                if window_id == windows.main {
                    if let Ok(state) = end_state.read() {
                        cfg.team_number = state.ds.team_number();
                        log::info!("Updating team number to {}", cfg.team_number);
                        if let Err(e) = confy::store("conductor", cfg) {
                            log::error!("Failed to store config: {}", e);
                        }
                    }
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::MainEventsCleared => (),
            _ => (),
        }
    });

    Ok(())
}
