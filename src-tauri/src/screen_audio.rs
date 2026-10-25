//! Audio del sistema por aplicacion al compartir pantalla.
//!
//! En lugar del feature de Chromium (que solo captura el sink por defecto completo, incluido
//! el audio del propio cliente), creamos una fuente virtual de PipeWire "vesk-screen-share"
//! (nodo Audio/Source/Virtual, igual que venmic) y enlazamos solo los streams de audio
//! elegidos. El cliente de Vesk siempre se excluye para evitar eco/audio duplicado.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::time::Duration;

use pipewire::{
    properties::properties,
    types::ObjectType,
    main_loop::MainLoop,
    context::Context,
    core::Core,
    link::Link,
    node::Node,
    spa::utils::dict::DictRef,
};

/// Modo de captura de audio al compartir pantalla.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Sin captura de audio.
    Off,
    /// Todo el sistema excepto el propio cliente de Vesk.
    System,
    /// Solo la aplicacion cuyo nombre/contiene el texto.
    App(String),
}

/// Aplicacion con audio de salida, para el selector de la UI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AppInfo {
    pub id: u32,
    pub name: String,
}

enum Command {
    SetMode(Mode),
}

struct NodeInfo {
    name: String,
    binary: String,
    pid: u32,
    output_ports: Vec<u32>,
}

struct State {
    core: Core,
    nodes: HashMap<u32, NodeInfo>,
    virtual_id: Option<u32>,
    virtual_inputs: Vec<u32>,
    virtual_node: Option<Node>,
    links: Vec<Link>,
    mode: Mode,
    our_pid: u32,
    our_binary: String,
}

impl State {
    fn new(core: Core, mode: Mode) -> Self {
        let our_binary = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        State {
            core,
            nodes: HashMap::new(),
            virtual_id: None,
            virtual_inputs: Vec::new(),
            virtual_node: None,
            links: Vec::new(),
            mode,
            our_pid: std::process::id(),
            our_binary,
        }
    }
}

fn prop<'a>(dict: &'a DictRef, key: &str) -> Option<&'a str> {
    dict.get(key)
}

fn on_global(state: &Rc<RefCell<State>>, apps: &Arc<Mutex<Vec<AppInfo>>>, global: &pipewire::registry::GlobalObject<&DictRef>) {
    let props = match global.props {
        Some(p) => p,
        None => return,
    };

    match global.type_ {
        ObjectType::Node => {
            let name = prop(props, "node.name").unwrap_or("").to_string();
            if name == "vesk-screen-share" {
                state.borrow_mut().virtual_id = Some(global.id);
                relink(state);
                return;
            }

            let binary = prop(props, "application.process.binary").unwrap_or("").to_string();
            let pid = prop(props, "application.process.id").and_then(|s| s.parse().ok()).unwrap_or(0);
            let display = prop(props, "application.name")
                .or_else(|| prop(props, "media.name"))
                .unwrap_or(&name)
                .to_string();

            {
                let mut st = state.borrow_mut();
                let info = st.nodes.entry(global.id).or_insert(NodeInfo {
                    name: display.clone(),
                    binary: binary.clone(),
                    pid,
                    output_ports: Vec::new(),
                });
                info.name = display;
                info.binary = binary;
                info.pid = pid;
            }
            update_apps(state, apps);
            relink(state);
        }
        ObjectType::Port => {
            let node_id = match prop(props, "object.node").and_then(|s| s.parse::<u32>().ok()) {
                Some(v) => v,
                None => return,
            };
            let port_id = match prop(props, "port.id").and_then(|s| s.parse::<u32>().ok()) {
                Some(v) => v,
                None => return,
            };
            let dir: i32 = prop(props, "port.direction").and_then(|s| s.parse().ok()).unwrap_or(0);

            {
                let mut st = state.borrow_mut();
                if st.virtual_id == Some(node_id) {
                    if dir == 0 && !st.virtual_inputs.contains(&port_id) {
                        st.virtual_inputs.push(port_id);
                    }
                } else if let Some(info) = st.nodes.get_mut(&node_id) {
                    if dir == 1 && !info.output_ports.contains(&port_id) {
                        info.output_ports.push(port_id);
                    }
                } else {
                    return;
                }
            }
            relink(state);
        }
        _ => {}
    }
}

fn on_remove(state: &Rc<RefCell<State>>, id: u32) {
    {
        let mut st = state.borrow_mut();
        if st.virtual_id == Some(id) {
            st.virtual_id = None;
            st.virtual_inputs.clear();
            st.virtual_node = None;
        }
        st.nodes.remove(&id);
    }
    relink(state);
}

fn is_capture_target(st: &State, info: &NodeInfo) -> bool {
    if info.binary == st.our_binary || info.pid == st.our_pid {
        return false;
    }
    match &st.mode {
        Mode::Off => false,
        Mode::System => true,
        Mode::App(filter) => info.name.contains(filter) || info.binary.contains(filter),
    }
}

fn relink(state: &Rc<RefCell<State>>) {
    let specs = {
        let st = state.borrow();
        if st.mode == Mode::Off || st.virtual_id.is_none() || st.virtual_inputs.is_empty() {
            Vec::new()
        } else {
            let vnode = st.virtual_id.unwrap();
            let vins = st.virtual_inputs.clone();
            let mut specs: Vec<(u32, u32, u32, u32)> = Vec::new();
            for (nid, info) in &st.nodes {
                if !is_capture_target(&st, info) {
                    continue;
                }
                for (i, &op) in info.output_ports.iter().enumerate() {
                    let vin = vins[i % vins.len()];
                    specs.push((*nid, op, vin, vnode));
                }
            }
            specs
        }
    };

    state.borrow_mut().links.clear();

    if specs.is_empty() {
        return;
    }

    let new_links = {
        let core = state.borrow().core.clone();
        let mut created = Vec::new();
        for (nid, op, vin, vnode) in &specs {
            let link_props = properties! {
                "link.output.node" => nid.to_string(),
                "link.output.port" => op.to_string(),
                "link.input.node" => vnode.to_string(),
                "link.input.port" => vin.to_string(),
                "link.passive" => "true",
            };
            if let Ok(link) = core.create_object::<Link>("link-factory", &link_props) {
                created.push(link);
            }
        }
        created
    };

    state.borrow_mut().links = new_links;
}

fn update_apps(state: &Rc<RefCell<State>>, apps: &Arc<Mutex<Vec<AppInfo>>>) {
    let st = state.borrow();
    let list: Vec<AppInfo> = st
        .nodes
        .values()
        .filter(|n| n.binary != st.our_binary && n.pid != st.our_pid)
        .map(|n| AppInfo { id: 0, name: n.name.clone() })
        .collect();
    drop(st);
    *apps.lock().unwrap() = list;
}

fn apply_mode(state: &Rc<RefCell<State>>, mode: Mode) {
    {
        let mut st = state.borrow_mut();
        st.mode = mode;
    }

    if matches!(state.borrow().mode, Mode::Off) {
        let mut st = state.borrow_mut();
        st.virtual_id = None;
        st.virtual_inputs.clear();
        st.virtual_node = None;
        st.links.clear();
        return;
    }

    let need_create = state.borrow().virtual_id.is_none();
    if need_create {
        let props = properties! {
            "factory.name" => "support.null-audio-sink",
            "media.class" => "Audio/Source/Virtual",
            "node.name" => "vesk-screen-share",
            "node.description" => "Vesk Screen Share",
            "device.class" => "sound",
            "device.icon_name" => "audio-input-microphone",
            "audio.channels" => "2",
            "audio.position" => "FL,FR",
        };
        if let Ok(node) = state.borrow().core.create_object::<Node>("adapter", &props) {
            state.borrow_mut().virtual_node = Some(node);
        }
    }

    relink(state);
}

fn initial_mode() -> Mode {
    if std::env::var("VESK_SCREEN_AUDIO").as_deref() == Ok("0") {
        return Mode::Off;
    }
    match std::env::var("VESK_SCREEN_AUDIO_APP").as_deref() {
        Ok("off") | Ok("none") => Mode::Off,
        Ok("system") => Mode::System,
        Ok(app) if !app.is_empty() => Mode::App(app.to_string()),
        _ => Mode::System,
    }
}

struct Engine {
    apps: Arc<Mutex<Vec<AppInfo>>>,
    commands: Arc<Mutex<Vec<Command>>>,
    quit: Arc<AtomicBool>,
}

static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();

/// Arranca el motor de audio (si no esta activo). Es seguro llamarlo varias veces.
pub fn init() {
    if ENGINE.get().is_some() {
        return;
    }

    let apps = Arc::new(Mutex::new(Vec::new()));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let quit = Arc::new(AtomicBool::new(false));
    let mode = Arc::new(Mutex::new(initial_mode()));

    thread::spawn({
        let apps = apps.clone();
        let commands = commands.clone();
        let quit = quit.clone();
        let mode = mode.clone();
        move || run_thread(apps, commands, quit, mode)
    });

    let _ = ENGINE.set(Engine { apps, commands, quit });
}

fn run_thread(apps: Arc<Mutex<Vec<AppInfo>>>, commands: Arc<Mutex<Vec<Command>>>, quit: Arc<AtomicBool>, mode: Arc<Mutex<Mode>>) {
    let mainloop = match MainLoop::new(None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[vesk] no se pudo iniciar PipeWire (audio de pantalla desactivado): {e}");
            return;
        }
    };
    let context = match Context::new(&mainloop) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vesk] no se pudo conectar a PipeWire: {e}");
            return;
        }
    };
    let core = match context.connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vesk] no se pudo conectar al core de PipeWire: {e}");
            return;
        }
    };
    let registry = match core.get_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[vesk] no se pudo obtener el registro de PipeWire: {e}");
            return;
        }
    };

    let state = Rc::new(RefCell::new(State::new(core, mode.lock().unwrap().clone())));
    let apps2 = apps.clone();
    let st_global = Rc::clone(&state);
    let st_remove = Rc::clone(&state);
    // El Listener mantiene vivas las callbacks mientras el hilo corre; se guarda
    // en una variable local (no en State) porque tiene una lifetime ligada al
    // registry y no puede guardarse dentro de State.
    let _listener = registry
        .add_listener_local()
        .global(move |global| on_global(&st_global, &apps2, global))
        .global_remove(move |id| on_remove(&st_remove, id))
        .register();

    apply_mode(&state, state.borrow().mode.clone());

    while !quit.load(Ordering::Relaxed) {
        mainloop.loop_().iterate(Duration::from_millis(120));

        let pending: Vec<Command> = {
            let mut q = commands.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for cmd in pending {
            let Command::SetMode(m) = cmd;
            *mode.lock().unwrap() = m.clone();
            apply_mode(&state, m);
        }
    }
}

/// Cambia el modo de captura.
pub fn set_mode(mode: Mode) {
    if let Some(engine) = ENGINE.get() {
        engine.commands.lock().unwrap().push(Command::SetMode(mode));
    }
}

/// Lista las aplicaciones con audio de salida (excluye Vesk).
pub fn list_apps() -> Vec<AppInfo> {
    ENGINE
        .get()
        .map(|e| e.apps.lock().unwrap().clone())
        .unwrap_or_default()
}

/// Detiene el motor (al cerrar la app).
pub fn shutdown() {
    if let Some(engine) = ENGINE.get() {
        engine.quit.store(true, Ordering::Relaxed);
    }
}
