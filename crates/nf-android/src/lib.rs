//! Thin JNI + Vulkan (`wgpu`) bridge (`libdxandroid.so`) between Kotlin/Jetpack Compose
//! (`com.nightfire.android.NativeBridge`) and `nf-view` / `nf-game` / `nf-audio`.

#[cfg(target_os = "android")]
mod android_impl {
    use glam::Vec3;
    use jni::objects::{JClass, JObject, JString};
    use jni::sys::{jboolean, jfloat, jint, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;
    use nf_view::driving::{driving_viv_for_level, load_driving_scene, DrivingState};
    use nf_view::gpu::{self, HudFrame, Renderer};
    use nf_view::menu::{self, Action as MenuAction, FrontEnd};
    use nf_view::scene::{Scene, ViewOpts};
    use nf_view::{
        build_sound, cam_axes, dead_destroy_centres, device, gpu_instance_headless, load_front_end,
        load_with_carry, overlay_for, shot_hud, sound_tick, wreck_draw_centres, Camera, GAME_FOV,
    };
    use raw_window_handle::{
        AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::ptr::NonNull;
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const BTN_FIRE: i32 = 1 << 0;
    const BTN_AIM: i32 = 1 << 1;
    const BTN_RELOAD: i32 = 1 << 2;
    const BTN_JUMP: i32 = 1 << 3;
    const BTN_CROUCH: i32 = 1 << 4;
    const BTN_USE: i32 = 1 << 5;
    const BTN_WEAP_NEXT: i32 = 1 << 6;
    const BTN_WEAP_PREV: i32 = 1 << 7;
    const BTN_PAUSE: i32 = 1 << 8;

    static STATE: Mutex<Option<AndroidApp>> = Mutex::new(None);
    static LIVE_BRIDGE: OnceLock<Mutex<nf_view::live_bridge::LiveBridge>> = OnceLock::new();
    static TELEMETRY_TX: OnceLock<SyncSender<String>> = OnceLock::new();
    static CONTROL_QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

    fn ensure_live_bridge() {
        if LIVE_BRIDGE.get().is_some() {
            return;
        }
        match nf_view::live_bridge::LiveBridge::bind(9878) {
            Ok(b) => {
                log::info!("LiveBridge listening on 127.0.0.1:{}", b.port());
                let _ = LIVE_BRIDGE.set(Mutex::new(b));
            }
            Err(e) => {
                log::warn!("LiveBridge bind(9878) failed: {e}");
            }
        }
    }

    /// Debug telemetry + remote control. Off unless the Kotlin side passes `enable`, which it does
    /// only for a debuggable build launched with the `nf_telemetry` intent extra (see MainActivity).
    /// Both sockets are loopback only: 127.0.0.1:9876 out (reach the host with `adb reverse`),
    /// 127.0.0.1:9877 in (reach it with `adb forward`).
    fn start_telemetry_once(enable: bool) {
        if !enable {
            return;
        }
        TELEMETRY_TX.get_or_init(|| {
            // Control listener on 127.0.0.1:9877 (adb forward tcp:9877 tcp:9877)
            std::thread::spawn(|| {
                if let Ok(listener) = TcpListener::bind("127.0.0.1:9877") {
                    for conn in listener.incoming().flatten() {
                        let reader = BufReader::new(conn);
                        for line in reader.lines().map_while(Result::ok) {
                            let cmd = line.trim().to_string();
                            if !cmd.is_empty() {
                                CONTROL_QUEUE.lock().unwrap().push_back(cmd);
                            }
                        }
                    }
                }
            });

            let (tx, rx) = sync_channel::<String>(256);
            std::thread::spawn(move || {
                let mut stream: Option<TcpStream> = None;
                while let Ok(line) = rx.recv() {
                    if stream.is_none() {
                        stream = TcpStream::connect_timeout(
                            &"127.0.0.1:9876".parse().unwrap(),
                            Duration::from_millis(200),
                        )
                        .ok();
                        if let Some(s) = stream.as_ref() {
                            let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                        }
                    }
                    if let Some(s) = stream.as_mut() {
                        if s.write_all(line.as_bytes()).is_err()
                            || s.write_all(b"\n").is_err()
                        {
                            stream = None;
                        }
                    }
                }
            });
            tx
        });
    }

    fn emit_telemetry(json: String) {
        if let Some(tx) = TELEMETRY_TX.get() {
            let _ = tx.try_send(json);
        }
    }

    fn json_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => out.push(' '),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    struct NativeGfx {
        window: *mut ndk_sys::ANativeWindow,
        _instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        depth: wgpu::TextureView,
        renderer: Renderer,
    }

    unsafe impl Send for NativeGfx {}

    impl Drop for NativeGfx {
        fn drop(&mut self) {
            // Drop surface before releasing ANativeWindow.
            unsafe {
                if !self.window.is_null() {
                    ndk_sys::ANativeWindow_release(self.window);
                    self.window = std::ptr::null_mut();
                }
            }
        }
    }

    #[derive(Default)]
    struct TouchInput {
        move_x: f32,
        move_y: f32,
        turn_accum: f32,
        look_accum: f32,
        buttons: i32,
        prev_buttons: i32,
        pending_menu_actions: Vec<MenuAction>,
    }

    struct AndroidApp {
        data_path: String,
        archive: String,
        scene: Scene,
        sound: Option<nf_audio::sfx::Sound>,
        cam: Camera,
        opts: ViewOpts,
        fe: Option<FrontEnd>,
        driving: Option<DrivingState>,
        driving_end_ticks: Option<u32>,
        lang: Option<nf_formats::hud::Lang>,
        gfx: Option<NativeGfx>,
        input: TouchInput,
        last_frame: Instant,
        accum_secs: f64,
        ticks: u32,
        movie: Option<nf_view::movie::Player>,
        movie_failed: Option<u32>,
    }

    // Scene's Option<Box<dyn MpSession>> is never used on a separate thread; all access goes through STATE Mutex.
    unsafe impl Send for AndroidApp {}

    fn init_logger_once(telemetry: bool) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            android_logger::init_once(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Info)
                    .with_tag("NightfirePort"),
            );
            start_telemetry_once(telemetry);
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let msg = format!("{info}");
                log::error!("PANIC: {msg}");
                emit_telemetry(format!("{{\"ev\":\"panic\",\"msg\":{}}}", json_escape(&msg)));
                prev(info);
            }));
        });
    }

    fn load_lang(data_path: &str) -> Option<nf_formats::hud::Lang> {
        let iso = nf_formats::iso9660::IsoImage::open(data_path).ok()?;
        let (list, files_bin) = nf_formats::disc::files_bin_entries(&iso).ok()?;
        let entry = list.iter().find(|e| e.name.eq_ignore_ascii_case("USATxt.dat"))?;
        let base = files_bin.lba as u64 * 2048;
        let bytes = iso.read_bytes(base + entry.offset as u64, entry.size as u64).ok()?;
        nf_formats::hud::Lang::parse(&bytes).ok()
    }

    fn driving_viv_from_name(archive: &str) -> Option<&'static str> {
        let stem = archive
            .trim_end_matches(".bin")
            .trim_end_matches(".BIN")
            .trim_end_matches(".viv")
            .trim_end_matches(".VIV");
        if let Ok(code) = u32::from_str_radix(stem, 16) {
            if let Some(v) = driving_viv_for_level(code) {
                return Some(v);
            }
        }
        match stem.to_ascii_uppercase().as_str() {
            "MIS01" => Some("MIS01"),
            "MIS3" => Some("MIS3"),
            "MIS4" => Some("MIS4"),
            "MIS11" => Some("MIS11"),
            "MIS13A" => Some("MIS13A"),
            "MIS13B" => Some("MIS13B"),
            "MIS13C" => Some("MIS13C"),
            "RACE" => Some("RACE"),
            _ => None,
        }
    }

    /// A level name is a bare file name (`07000001.bin`, `MIS01`): no path separators or `..`,
    /// so neither the UI nor the debug control socket can make the loader read outside the data dir.
    fn is_safe_archive_name(archive: &str) -> bool {
        !archive.is_empty()
            && archive.len() <= 32
            && archive.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
            && !archive.contains("..")
    }

    fn load_scene_for(
        data_path: &str,
        archive: &str,
    ) -> anyhow::Result<(Scene, Camera, Option<FrontEnd>, Option<DrivingState>)> {
        if !is_safe_archive_name(archive) {
            anyhow::bail!("rejected level name {archive:?}");
        }
        if let Some(viv) = driving_viv_from_name(archive) {
            let (scene, drv) = load_driving_scene(data_path, viv)?;
            let (p, yaw, pitch) = drv.chase_camera();
            let cam = Camera {
                pos: Vec3::new(p[0], p[1], p[2]),
                yaw,
                pitch,
                fov: GAME_FOV,
            };
            return Ok((scene, cam, None, Some(drv)));
        }
        let is_fe = archive.eq_ignore_ascii_case("07000048.bin");
        let scene = if is_fe {
            load_front_end(data_path, archive, true, None, false, 1, 0)?
        } else {
            load_with_carry(data_path, archive, true, None)?
        };
        let cam = if let Some(sim) = &scene.sim {
            let p = sim.bond.pos;
            Camera {
                pos: Vec3::new(p[0], p[1] + 1.65, p[2]),
                yaw: sim.bond.yaw,
                pitch: sim.bond.pitch,
                fov: GAME_FOV,
            }
        } else {
            Camera {
                pos: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
                fov: GAME_FOV,
            }
        };
        let fe = if is_fe {
            let mut f = FrontEnd::new();
            f.unlock_all = true;
            let profile_dir = std::path::Path::new(data_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new(data_path))
                .join("profiles");
            let _ = std::fs::create_dir_all(&profile_dir);
            f.set_profile_dir(profile_dir);
            Some(f)
        } else {
            None
        };
        Ok((scene, cam, fe, None))
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeSetContext(
        env: JNIEnv,
        _cls: JClass,
        context: JObject,
    ) {
        static CTX_ONCE: std::sync::Once = std::sync::Once::new();
        CTX_ONCE.call_once(|| {
            if let (Ok(vm), Ok(global)) = (env.get_java_vm(), env.new_global_ref(context)) {
                let vm_ptr = vm.get_java_vm_pointer() as *mut std::ffi::c_void;
                let ctx_ptr = global.as_raw() as *mut std::ffi::c_void;
                std::mem::forget(global);
                unsafe {
                    ndk_context::initialize_android_context(vm_ptr, ctx_ptr);
                }
            }
        });
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeInit(
        mut env: JNIEnv,
        _cls: JClass,
        data_path: JString,
        telemetry: jboolean,
    ) -> jboolean {
        init_logger_once(telemetry != JNI_FALSE);
        let path: String = match env.get_string(&data_path) {
            Ok(s) => s.into(),
            Err(e) => {
                log::error!("nativeInit: bad data_path: {e}");
                return JNI_FALSE;
            }
        };
        log::info!("nativeInit: data_path={path}");
        let start_archive = if std::path::Path::new(&path)
            .join("FILES")
            .join("07000048.bin")
            .is_file()
        {
            "07000048.bin"
        } else {
            "07000001.bin"
        };
        let t0 = Instant::now();
        let lang = load_lang(&path);
        match load_scene_for(&path, start_archive) {
            Ok((mut scene, cam, fe, driving)) => {
                let sound = build_sound(&mut scene);
                let ms = t0.elapsed().as_millis();
                emit_telemetry(format!(
                    "{{\"ev\":\"boot\",\"archive\":{},\"ok\":true,\"ms\":{ms},\"data_path\":{},\"cels\":{},\"textures\":{}}}",
                    json_escape(start_archive),
                    json_escape(&path),
                    scene.cels.len(),
                    scene.textures.len()
                ));
                ensure_live_bridge();
                let app = AndroidApp {
                    data_path: path,
                    archive: start_archive.to_string(),
                    scene,
                    sound,
                    cam,
                    opts: ViewOpts::default(),
                    fe,
                    driving,
                    driving_end_ticks: None,
                    lang,
                    gfx: None,
                    input: TouchInput::default(),
                    last_frame: Instant::now(),
                    accum_secs: 0.0,
                    ticks: 0,
                    movie: None,
                    movie_failed: None,
                };
                *STATE.lock().unwrap() = Some(app);
                log::info!("nativeInit: loaded {start_archive} successfully in {ms}ms");
                JNI_TRUE
            }
            Err(e) => {
                let ms = t0.elapsed().as_millis();
                let err_s = format!("{e:#}");
                emit_telemetry(format!(
                    "{{\"ev\":\"boot\",\"archive\":{},\"ok\":false,\"ms\":{ms},\"data_path\":{},\"err\":{}}}",
                    json_escape(start_archive),
                    json_escape(&path),
                    json_escape(&err_s)
                ));
                log::error!("nativeInit failed to load {start_archive}: {err_s}");
                JNI_FALSE
            }
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeSurfaceCreated(
        env: JNIEnv,
        _cls: JClass,
        surface_obj: JObject,
        width: jint,
        height: jint,
    ) -> jboolean {
        init_logger_once(false);
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else {
            log::error!("nativeSurfaceCreated called before nativeInit");
            return JNI_FALSE;
        };

        let win = unsafe {
            ndk_sys::ANativeWindow_fromSurface(env.get_raw() as *mut _, surface_obj.as_raw())
        };
        if win.is_null() {
            log::error!("ANativeWindow_fromSurface returned null");
            return JNI_FALSE;
        }

        let w = (width.max(1)) as u32;
        let h = (height.max(1)) as u32;

        let instance = gpu_instance_headless();
        let raw_win = RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(
            NonNull::new(win as *mut std::ffi::c_void).unwrap(),
        ));
        let raw_disp = RawDisplayHandle::Android(AndroidDisplayHandle::new());
        let surface = match unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_disp),
                raw_window_handle: raw_win,
            })
        } {
            Ok(s) => s,
            Err(e) => {
                log::error!("create_surface_unsafe failed: {e}");
                unsafe { ndk_sys::ANativeWindow_release(win) };
                return JNI_FALSE;
            }
        };

        let (adapter, device, queue) = match pollster::block_on(device(&instance, Some(&surface))) {
            Ok(d) => d,
            Err(e) => {
                log::error!("wgpu device creation failed: {e}");
                drop(surface);
                unsafe { ndk_sys::ANativeWindow_release(win) };
                return JNI_FALSE;
            }
        };

        let mut config = surface
            .get_default_config(&adapter, w, h)
            .expect("surface has no default config");
        let format = config.format;
        config.present_mode = wgpu::PresentMode::Fifo;
        surface.configure(&device, &config);
        let depth = gpu::depth_view(&device, w, h);
        let renderer = Renderer::new(&device, &queue, format, &app.scene, false, false);

        app.gfx = Some(NativeGfx {
            window: win,
            _instance: instance,
            surface,
            device,
            queue,
            config,
            depth,
            renderer,
        });
        let gpu_info = nf_view::bugreport::GPU_INFO.get().map(String::as_str).unwrap_or("");
        emit_telemetry(format!(
            "{{\"ev\":\"surface\",\"w\":{w},\"h\":{h},\"format\":\"{format:?}\",\"gpu\":{}}}",
            json_escape(gpu_info)
        ));
        log::info!("nativeSurfaceCreated: {w}x{h} format={format:?}");
        JNI_TRUE
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeSurfaceChanged(
        _env: JNIEnv,
        _cls: JClass,
        width: jint,
        height: jint,
    ) {
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else { return };
        let Some(gfx) = app.gfx.as_mut() else { return };
        let w = width.max(1) as u32;
        let h = height.max(1) as u32;
        if gfx.config.width != w || gfx.config.height != h {
            gfx.config.width = w;
            gfx.config.height = h;
            gfx.surface.configure(&gfx.device, &gfx.config);
            gfx.depth = gpu::depth_view(&gfx.device, w, h);
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeSurfaceDestroyed(
        _env: JNIEnv,
        _cls: JClass,
    ) {
        let mut guard = STATE.lock().unwrap();
        if let Some(app) = guard.as_mut() {
            app.gfx = None;
            log::info!("nativeSurfaceDestroyed");
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeLoadLevel(
        mut env: JNIEnv,
        _cls: JClass,
        archive: JString,
    ) -> jboolean {
        let name: String = match env.get_string(&archive) {
            Ok(s) => s.into(),
            Err(_) => return JNI_FALSE,
        };
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else {
            return JNI_FALSE;
        };
        let t0 = Instant::now();
        match load_scene_for(&app.data_path, &name) {
            Ok((scene, cam, fe, driving)) => {
                let ms = t0.elapsed().as_millis();
                emit_telemetry(format!(
                    "{{\"ev\":\"level_load\",\"archive\":{},\"ok\":true,\"ms\":{ms},\"cels\":{},\"textures\":{},\"sim\":{}}}",
                    json_escape(&name),
                    scene.cels.len(),
                    scene.textures.len(),
                    scene.sim.is_some()
                ));
                app.archive = name.clone();
                app.scene = scene;
                app.cam = cam;
                app.fe = fe;
                app.driving = driving;
                app.ticks = 0;
                if let Some(gfx) = app.gfx.as_mut() {
                    gfx.renderer = Renderer::new(
                        &gfx.device,
                        &gfx.queue,
                        gfx.config.format,
                        &app.scene,
                        false,
                        false,
                    );
                }
                log::info!("nativeLoadLevel: {name} in {ms}ms");
                JNI_TRUE
            }
            Err(e) => {
                let ms = t0.elapsed().as_millis();
                let err_s = format!("{e:#}");
                emit_telemetry(format!(
                    "{{\"ev\":\"level_load\",\"archive\":{},\"ok\":false,\"ms\":{ms},\"err\":{}}}",
                    json_escape(&name),
                    json_escape(&err_s)
                ));
                log::error!("nativeLoadLevel({name}) failed: {err_s}");
                JNI_FALSE
            }
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeSetInput(
        _env: JNIEnv,
        _cls: JClass,
        move_x: jfloat,
        move_y: jfloat,
        look_dx: jfloat,
        look_dy: jfloat,
        buttons_mask: jint,
    ) {
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else { return };
        app.input.move_x = move_x.clamp(-1.0, 1.0);
        app.input.move_y = move_y.clamp(-1.0, 1.0);
        app.input.turn_accum -= look_dx * 0.004;
        app.input.look_accum -= look_dy * 0.004 / std::f32::consts::FRAC_PI_2;
        app.input.buttons = buttons_mask;
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeMenuAction(
        _env: JNIEnv,
        _cls: JClass,
        action_id: jint,
    ) {
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else { return };
        let act = match action_id {
            0 => MenuAction::Up,
            1 => MenuAction::Down,
            2 => MenuAction::Left,
            3 => MenuAction::Right,
            4 => MenuAction::Confirm,
            5 => MenuAction::Back,
            6 => MenuAction::Side,
            7 => MenuAction::Extra,
            _ => return,
        };
        app.input.pending_menu_actions.push(act);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeStepAndRender(
        _env: JNIEnv,
        _cls: JClass,
    ) -> jint {
        let mut guard = STATE.lock().unwrap();
        let Some(app) = guard.as_mut() else {
            return -1;
        };

        let now = Instant::now();
        let dt = (now - app.last_frame).as_secs_f64().min(0.1);
        app.last_frame = now;
        app.accum_secs += dt;

        let mut pending_level: Option<String> = None;

        // Drain zero-restart Live Agent Bridge commands on 127.0.0.1:9878 (`adb forward tcp:9878 tcp:9878`)
        if let Some(reqs) = LIVE_BRIDGE.get().and_then(|m| m.lock().ok()).map(|b| b.drain()) {
            use nf_view::live_bridge::{escape_json, LiveCmd};
            for req in reqs {
                let reply = match &req.cmd {
                    LiveCmd::Ping => "{\"ok\":true,\"pong\":true,\"platform\":\"android\"}".into(),
                    LiveCmd::State => {
                        let pos = if let Some(drv) = app.driving.as_ref() {
                            drv.car.pos
                        } else if let Some(sim) = app.scene.sim.as_ref() {
                            sim.bond.pos
                        } else {
                            [app.cam.pos.x, app.cam.pos.y, app.cam.pos.z]
                        };
                        let hp = app.scene.sim.as_ref().map_or(100.0, |s| s.drones.health.health);
                        let weapon = app.scene.sim.as_ref().map_or(0, |s| s.gun.weapon);
                        format!(
                            "{{\"ok\":true,\"platform\":\"android\",\"archive\":\"{}\",\"pos\":[{:.3},{:.3},{:.3}],\"health\":{:.1},\"weapon\":{},\"driving\":{}}}",
                            escape_json(&app.archive), pos[0], pos[1], pos[2], hp, weapon, app.driving.is_some()
                        )
                    }
                    LiveCmd::Drones => {
                        let mut items = Vec::new();
                        if let Some(sim) = app.scene.sim.as_ref() {
                            for (i, d) in sim.drones.list.iter().enumerate() {
                                items.push(format!("{{\"i\":{},\"pos\":[{:.2},{:.2},{:.2}],\"hp\":{:.1}}}", i, d.pos[0], d.pos[1], d.pos[2], d.health));
                            }
                        }
                        format!("{{\"ok\":true,\"count\":{},\"drones\":[{}]}}", items.len(), items.join(","))
                    }
                    LiveCmd::Doors => {
                        let mut items = Vec::new();
                        if let Some(sim) = app.scene.sim.as_ref() {
                            for (i, d) in sim.drones.doors.list.iter().enumerate() {
                                items.push(format!("{{\"i\":{},\"pos\":[{:.2},{:.2},{:.2}],\"state\":{}}}", i, d.pos[0], d.pos[1], d.pos[2], d.state));
                            }
                        }
                        format!("{{\"ok\":true,\"count\":{},\"doors\":[{}]}}", items.len(), items.join(","))
                    }
                    LiveCmd::Driving => {
                        if let Some(drv) = app.driving.as_ref() {
                            format!("{{\"ok\":true,\"viv\":\"{}\",\"ticks\":{},\"pos\":[{:.2},{:.2},{:.2}],\"pov\":{},\"in_heli\":{}}}", drv.viv, drv.ticks, drv.car.pos[0], drv.car.pos[1], drv.car.pos[2], drv.pov_active(), drv.in_heli)
                        } else {
                            "{\"ok\":false,\"err\":\"not in driving mode\"}".into()
                        }
                    }
                    LiveCmd::Teleport { x, y, z, yaw, pitch } => {
                        if let Some(drv) = app.driving.as_mut() {
                            drv.car.pos = [*x, *y, *z];
                            drv.car.vel = [0.0; 3];
                            if let Some(yw) = yaw { drv.car.yaw = *yw; }
                        } else if let Some(sim) = app.scene.sim.as_mut() {
                            sim.bond.pos = [*x, *y, *z];
                            sim.bond.vel = [0.0; 3];
                            if let Some(yw) = yaw { sim.bond.yaw = *yw; }
                            if let Some(pt) = pitch { sim.bond.pitch = *pt; }
                        }
                        app.cam.pos = Vec3::new(*x, *y, *z);
                        format!("{{\"ok\":true,\"pos\":[{x:.2},{y:.2},{z:.2}]}}")
                    }
                    LiveCmd::LoadLevel(code) => {
                        pending_level = Some(format!("{code:08x}.bin"));
                        format!("{{\"ok\":true,\"loading\":\"{:08x}.bin\"}}", code)
                    }
                    LiveCmd::SetHealth(hp) => {
                        if let Some(sim) = app.scene.sim.as_mut() {
                            sim.drones.health.health = *hp;
                            format!("{{\"ok\":true,\"health\":{hp:.1}}}")
                        } else {
                            "{\"ok\":false,\"err\":\"no active sim\"}".into()
                        }
                    }
                    LiveCmd::GiveWeapon(id) => {
                        if let Some(sim) = app.scene.sim.as_mut() {
                            sim.gun.weapon = *id;
                            format!("{{\"ok\":true,\"weapon\":{id}}}")
                        } else {
                            "{\"ok\":false,\"err\":\"no active sim\"}".into()
                        }
                    }
                    LiveCmd::MoveDrone { index, x, y, z } => {
                        if let Some(d) = app.scene.sim.as_mut().and_then(|s| s.drones.list.get_mut(*index)) {
                            d.pos = [*x, *y, *z];
                            format!("{{\"ok\":true,\"drone\":{index},\"pos\":[{x:.2},{y:.2},{z:.2}]}}")
                        } else {
                            "{\"ok\":false,\"err\":\"drone index out of range\"}".into()
                        }
                    }
                    LiveCmd::KillDrone(index) => {
                        if let Some(d) = app.scene.sim.as_mut().and_then(|s| s.drones.list.get_mut(*index)) {
                            d.health = 0.0;
                            format!("{{\"ok\":true,\"killed\":{index}}}")
                        } else {
                            "{\"ok\":false,\"err\":\"drone index out of range\"}".into()
                        }
                    }
                    LiveCmd::OpenDoor(index) => {
                        if let Some(d) = app.scene.sim.as_mut().and_then(|s| s.drones.doors.list.get_mut(*index)) {
                            d.state = 1;
                            format!("{{\"ok\":true,\"door\":{index},\"state\":1}}")
                        } else {
                            "{\"ok\":false,\"err\":\"door index out of range\"}".into()
                        }
                    }
                    LiveCmd::SetMovieMode(mode) => {
                        if let Some(g) = app.gfx.as_mut() {
                            g.renderer.movie_presentation = match mode.as_str() {
                                "pillarbox" | "4:3" => nf_view::gpu::MoviePresentation::Pillarbox,
                                "cover" => nf_view::gpu::MoviePresentation::Cover,
                                _ => nf_view::gpu::MoviePresentation::Ambient,
                            };
                        }
                        format!("{{\"ok\":true,\"movie_mode\":\"{}\"}}", escape_json(mode))
                    }
                    LiveCmd::ReloadShader { target, path } => {
                        let src_res: Result<String, String> = if path.eq_ignore_ascii_case("default") {
                            match target.as_str() {
                                "hud" => Ok(nf_view::gpu::HUD_SHADER.to_string()),
                                "movie" => Ok(nf_view::gpu::MOVIE_SHADER.to_string()),
                                _ => Err(format!("unknown shader target '{target}'")),
                            }
                        } else {
                            std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))
                        };
                        match (src_res, app.gfx.as_mut()) {
                            (Ok(src), Some(g)) => match g.renderer.reload_shader(&g.device, target, &src) {
                                Ok(()) => format!("{{\"ok\":true,\"reloaded\":\"{}\"}}", escape_json(target)),
                                Err(e) => format!("{{\"ok\":false,\"err\":\"{}\"}}", escape_json(&e)),
                            },
                            (Err(e), _) => format!("{{\"ok\":false,\"err\":\"{}\"}}", escape_json(&e)),
                            (_, None) => "{\"ok\":false,\"err\":\"no active GPU renderer\"}".into(),
                        }
                    }
                    LiveCmd::Takeover(on) => {
                        if let Some(mut b) = LIVE_BRIDGE.get().and_then(|m| m.lock().ok()) {
                            b.control.active = *on;
                        }
                        format!("{{\"ok\":true,\"takeover\":{on}}}")
                    }
                    LiveCmd::Input { fwd, right, turn, look, fire, zoom, crouch, .. } => {
                        app.input.move_y = *fwd;
                        app.input.move_x = *right;
                        app.input.turn_accum += *turn;
                        app.input.look_accum += *look;
                        if *fire { app.input.buttons |= BTN_FIRE; } else { app.input.buttons &= !BTN_FIRE; }
                        if *zoom { app.input.buttons |= BTN_AIM; } else { app.input.buttons &= !BTN_AIM; }
                        if *crouch { app.input.buttons |= BTN_CROUCH; } else { app.input.buttons &= !BTN_CROUCH; }
                        "{\"ok\":true}".into()
                    }
                    LiveCmd::Menu(act) => {
                        let parsed = match act.as_str() {
                            "up" => Some(MenuAction::Up),
                            "down" => Some(MenuAction::Down),
                            "left" => Some(MenuAction::Left),
                            "right" => Some(MenuAction::Right),
                            "confirm" | "ok" | "x" | "select" => Some(MenuAction::Confirm),
                            "back" | "cancel" | "triangle" => Some(MenuAction::Back),
                            "side" | "square" => Some(MenuAction::Side),
                            "extra" | "circle" => Some(MenuAction::Extra),
                            _ => None,
                        };
                        if let Some(a) = parsed {
                            app.input.pending_menu_actions.push(a);
                            format!("{{\"ok\":true,\"menu\":\"{}\"}}", escape_json(act))
                        } else {
                            format!("{{\"ok\":false,\"err\":\"unknown menu action '{}'\"}}", escape_json(act))
                        }
                    }
                    LiveCmd::Shot(_) => "{\"ok\":false,\"err\":\"use adb screencap on Android\"}".into(),
                };
                req.respond(reply);
            }
        }

        // Drain debug control commands (127.0.0.1:9877; empty unless telemetry was enabled)
        while let Some(cmd) = CONTROL_QUEUE.lock().unwrap().pop_front() {
            if let Some(arch) = cmd.strip_prefix("load ") {
                pending_level = Some(arch.trim().to_string());
            } else if let Some(act_s) = cmd.strip_prefix("menu ") {
                if let Ok(id) = act_s.trim().parse::<i32>() {
                    let act = match id {
                        0 => Some(MenuAction::Up),
                        1 => Some(MenuAction::Down),
                        2 => Some(MenuAction::Left),
                        3 => Some(MenuAction::Right),
                        4 => Some(MenuAction::Confirm),
                        5 => Some(MenuAction::Back),
                        6 => Some(MenuAction::Side),
                        7 => Some(MenuAction::Extra),
                        _ => None,
                    };
                    if let Some(a) = act {
                        app.input.pending_menu_actions.push(a);
                    }
                }
            }
        }

        // Drain menu actions if FrontEnd is active
        let actions: Vec<MenuAction> = app.input.pending_menu_actions.drain(..).collect();
        if let Some(fe) = app.fe.as_mut() {
            for a in actions {
                if fe.flow.get_state() == menu::ST_MOVIE {
                    if matches!(a, MenuAction::Confirm | MenuAction::Back | MenuAction::Down) {
                        app.movie = None;
                        fe.flow.pop_state();
                    }
                } else {
                    fe.action(a);
                }
            }
            let menu_rm: Vec<u32> = fe.drain_sfx_removes().collect();
            let menu_sfx: Vec<menu::MenuSfx> = fe.drain_sfx().collect();
            if !menu_sfx.is_empty() || !menu_rm.is_empty() {
                if let Some(snd) = app.sound.as_mut() {
                    snd.batch(|drv, out| {
                        for id in &menu_rm {
                            if nf_view::menu_music::menu_sfx_reaches_driver(*id) {
                                drv.remove(0, Some(*id));
                            }
                        }
                        for ev in &menu_sfx {
                            if nf_view::menu_music::menu_sfx_reaches_driver(ev.id) {
                                drv.start(out, ev.id, None, 0, None);
                            }
                        }
                    });
                }
            }
        }

        // Fixed 60 Hz logic ticks (max 4 per frame)
        let mut ran = 0;
        while app.accum_secs >= 1.0 / 60.0 && ran < 4 {
            app.accum_secs -= 1.0 / 60.0;
            ran += 1;
            app.ticks = app.ticks.wrapping_add(1);

            if let Some(fe) = app.fe.as_mut() {
                fe.tick();
                if let Some(req) = fe.requested_level.take() {
                    pending_level = Some(format!("{req:08x}.bin"));
                } else if let Some(drv_code) = fe.launching_driving() {
                    pending_level = Some(format!("{drv_code:08x}.bin"));
                }
            }

            if let Some(drv) = app.driving.as_mut() {
                let b = app.input.buttons;
                let prev = app.input.prev_buttons;
                let just = b & !prev;
                app.input.prev_buttons = b;

                let throttle = (app.input.move_y.max(0.0) + if b & BTN_FIRE != 0 { 1.0 } else { 0.0 }).min(1.0);
                let brake = ((-app.input.move_y).max(0.0) + if b & BTN_AIM != 0 { 1.0 } else { 0.0 }).min(1.0);
                let steer = -app.input.move_x;
                let handbrake = b & BTN_JUMP != 0;
                if drv.pov_active() {
                    let (dx, dy) = (
                        std::mem::take(&mut app.input.turn_accum),
                        std::mem::take(&mut app.input.look_accum),
                    );
                    if dx != 0.0 || dy != 0.0 {
                        // Undo `nativeSetInput`'s 0.004 rad/px scaling so `pov_aim` receives raw touch pixels.
                        drv.pov_aim((-dx * 250.0) as f64, (-dy * 250.0 * std::f32::consts::FRAC_PI_2) as f64);
                    }
                    if b & BTN_FIRE != 0 {
                        drv.pov_fire();
                    }
                    drv.pov_scope(b & BTN_AIM != 0);
                } else if let Some(w) = drv.weapons.as_mut() {
                    w.fire |= b & BTN_FIRE != 0;
                    w.cycle |= just & BTN_RELOAD != 0;
                }
                drv.step(throttle, brake, steer, handbrake);
                app.cam.fov = drv.camera_fov().to_radians();

                if matches!(
                    drv.mission_state,
                    nf_game::driving::mission::MissionState::Complete
                        | nf_game::driving::mission::MissionState::Failed
                ) {
                    let t = app.driving_end_ticks.get_or_insert(0);
                    *t += 1;
                    if (*t >= 180 || (*t >= 20 && (just & BTN_USE != 0))) && pending_level.is_none() {
                        let state_str = match drv.mission_state {
                            nf_game::driving::mission::MissionState::Complete => "Complete",
                            nf_game::driving::mission::MissionState::Failed => "Failed",
                            nf_game::driving::mission::MissionState::Playing => "Playing",
                        };
                        emit_telemetry(format!(
                            "{{\"ev\":\"driving_end\",\"archive\":{},\"mission_state\":{},\"ticks\":{}}}",
                            json_escape(&app.archive),
                            json_escape(state_str),
                            drv.ticks
                        ));
                        // On failure, retry the current driving mission instead of kicking back to the Start screen;
                        // on completion, advance Paris Prelude -> The Exchange or return to FrontEnd.
                        pending_level = Some(if matches!(drv.mission_state, nf_game::driving::mission::MissionState::Failed) {
                            app.archive.clone()
                        } else if app.archive.eq_ignore_ascii_case("09000001.bin") || app.archive.eq_ignore_ascii_case("MIS01") {
                            "07000005.bin".to_string()
                        } else {
                            "07000048.bin".to_string()
                        });
                        app.driving_end_ticks = None;
                        break;
                    }
                } else {
                    app.driving_end_ticks = None;
                }
            } else if app.fe.is_none() {
                if let Some(sim) = app.scene.sim.as_mut() {
                    let b = app.input.buttons;
                    let prev = app.input.prev_buttons;
                    let just = b & !prev;
                    app.input.prev_buttons = b;

                    let weap_cycle = if just & BTN_WEAP_NEXT != 0 {
                        1
                    } else if just & BTN_WEAP_PREV != 0 {
                        -1
                    } else {
                        0
                    };

                    // Allow skipping in-mission NIS cutscenes (e.g. Equinox intro) via USE, JUMP, or PAUSE.
                    if sim.drones.scripts.cutscene_playing()
                        && (just & (BTN_USE | BTN_JUMP | BTN_PAUSE) != 0)
                    {
                        sim.drones.scripts.skip_request = true;
                    }
                    let in_keypad = sim.drones.keypad.is_some();
                    let keypad_nav = if in_keypad {
                        let mut bits = 0u8;
                        if app.input.move_y > 0.45 { bits |= 1; }
                        if app.input.move_y < -0.45 { bits |= 2; }
                        if app.input.move_x < -0.45 { bits |= 4; }
                        if app.input.move_x > 0.45 { bits |= 8; }
                        bits
                    } else {
                        0
                    };
                    let pi = nf_game::player::Input {
                        forward: if in_keypad { 0.0 } else { app.input.move_y },
                        strafe: if in_keypad { 0.0 } else { -app.input.move_x },
                        lift: if b & BTN_JUMP != 0 { 1.0 } else { 0.0 }
                            - if b & BTN_CROUCH != 0 { 1.0 } else { 0.0 },
                        stabilise: (b & (BTN_USE | BTN_RELOAD)) != 0,
                        turn: std::mem::take(&mut app.input.turn_accum),
                        look: std::mem::take(&mut app.input.look_accum),
                        jump: just & BTN_JUMP != 0,
                        crouch: just & BTN_CROUCH != 0,
                        crouch_hold: false,
                        crouch_held: false,
                        aiming: b & BTN_AIM != 0,
                        weapon_cycle: weap_cycle,
                        gadget_cycle: 0,
                        zoom_cycle: 0,
                        zoom_axis: 0.0,
                        switch_to: None,
                        fire: !in_keypad && (b & BTN_FIRE != 0),
                        zoom: b & BTN_AIM != 0,
                        scope: b & BTN_AIM != 0,
                        mode_switch: false,
                        goggles_action: false,
                        keypad_confirm: in_keypad && (b & (BTN_USE | BTN_FIRE) != 0),
                        keypad_back: in_keypad && (b & (BTN_CROUCH | BTN_RELOAD) != 0),
                        keypad_nav,
                    };
                    let fire = b & BTN_FIRE != 0;
                    let reload = just & BTN_RELOAD != 0;
                    let use_key = just & BTN_USE != 0;
                    sim.step_bond(pi, fire, reload, use_key);
                    if let Some(s) = app.sound.as_mut() {
                        sound_tick(s, sim);
                    }
                    if let Some(next) = sim.drones.level_exit.take() {
                        if sim.drones.exit_hold.is_some() {
                            sim.drones.level_exit = Some(next);
                        } else {
                            let wrapped = nf_game::triggers::wrap_intro(next);
                            let resolved = nf_game::triggers::resolve_exit_code(wrapped);
                            pending_level = Some(format!("{resolved:08x}.bin"));
                        }
                    }
                }
            } else if app.fe.is_some() || app.driving.is_some() {
                if let Some(s) = app.sound.as_mut() {
                    s.batch(|_, _| {});
                }
            }
        }

        if let Some(next_arch) = pending_level {
            if let Some(s) = app.sound.as_mut() {
                s.reset();
            }
            let t0 = Instant::now();
            match load_scene_for(&app.data_path, &next_arch) {
                Ok((mut scene, cam, fe, driving)) => {
                    let sound = build_sound(&mut scene);
                    let ms = t0.elapsed().as_millis();
                    emit_telemetry(format!(
                        "{{\"ev\":\"level_load\",\"archive\":{},\"ok\":true,\"ms\":{ms},\"cels\":{},\"textures\":{},\"sim\":{}}}",
                        json_escape(&next_arch),
                        scene.cels.len(),
                        scene.textures.len(),
                        scene.sim.is_some()
                    ));
                    app.archive = next_arch;
                    app.scene = scene;
                    app.sound = sound;
                    app.cam = cam;
                    app.fe = fe;
                    app.driving = driving;
                    app.driving_end_ticks = None;
                    app.ticks = 0;
                    if let Some(gfx) = app.gfx.as_mut() {
                        gfx.renderer = Renderer::new(
                            &gfx.device,
                            &gfx.queue,
                            gfx.config.format,
                            &app.scene,
                            false,
                            false,
                        );
                    }
                }
                Err(e) => {
                    let ms = t0.elapsed().as_millis();
                    let err_s = format!("{e:#}");
                    emit_telemetry(format!(
                        "{{\"ev\":\"level_load\",\"archive\":{},\"ok\":false,\"ms\":{ms},\"err\":{}}}",
                        json_escape(&next_arch),
                        json_escape(&err_s)
                    ));
                    log::error!("pending_level({next_arch}) failed: {err_s}");
                }
            }
        }

        // Update camera from driving or on-foot sim (1:1 with desktop nf-view)
        if let Some(drv) = &app.driving {
            let (p, yaw, pitch) = drv.chase_camera();
            app.cam.pos = Vec3::new(p[0], p[1], p[2]);
            app.cam.yaw = yaw;
            app.cam.pitch = pitch;
            app.cam.fov = drv.camera_fov().to_radians();
        } else if let Some(sim) = &app.scene.sim {
            let h = sim.bond.head();
            app.cam = Camera {
                pos: Vec3::new(h[0], h[1], h[2]),
                yaw: sim.bond.yaw,
                pitch: sim.bond.pitch * std::f32::consts::FRAC_PI_2,
                fov: nf_view::view_fov(sim),
            };
            if let Some((p, yaw, pitch, fov)) = sim.script_camera() {
                app.cam = Camera { pos: Vec3::new(p[0], p[1], p[2]), yaw, pitch, fov };
            }
            if let Some((p, d)) = sim.view_override() {
                app.cam = Camera {
                    pos: Vec3::new(p[0], p[1], p[2]),
                    yaw: d[0].atan2(d[2]),
                    pitch: d[1].clamp(-1.0, 1.0).asin(),
                    fov: GAME_FOV,
                };
            }
            if let Some((p, yaw, pitch, fov)) = sim.creep_camera() {
                app.cam = Camera { pos: Vec3::new(p[0], p[1], p[2]), yaw, pitch, fov: GAME_FOV / fov };
            }
            if let Some((p, yaw, pitch, fov)) = sim.wire_camera() {
                app.cam = Camera { pos: Vec3::new(p[0], p[1], p[2]), yaw, pitch, fov: GAME_FOV / fov };
            }
            if let Some((p, yaw, pitch, fov)) = sim.cam2_camera() {
                app.cam = Camera { pos: Vec3::new(p[0], p[1], p[2]), yaw, pitch, fov: GAME_FOV / fov };
            }
        }

        let (right, up) = cam_axes(&app.cam);
        app.opts.right = right;
        app.opts.up = up;

        let Some(gfx) = app.gfx.as_mut() else {
            return 1;
        };

        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                return 0;
            }
            _ => return 0,
        };

        let view = frame.texture.create_view(&Default::default());
        let aspect = gfx.config.width as f32 / gfx.config.height.max(1) as f32;
        let (vp, vp_rot) = app.cam.view_proj(aspect);

        let door_closed = app.scene.sim.as_ref().map(|s| s.drones.doors.closed_flags());
        let vis = gfx.renderer.cel_visibility(
            app.cam.pos.to_array(),
            vp,
            app.scene.sim.as_ref().map(|s| &s.world),
            door_closed.as_deref(),
        );
        let decal_cells = app.scene.decal_cell_entries(&gfx.renderer.skin_layout);
        let (mut pal, cols) = app.scene.skin_palette(
            &gfx.renderer.skin_layout,
            None,
            app.ticks as f32,
            app.opts,
            None,
        );
        if let Some(v) = vis.as_deref() {
            app.scene.hide_unseen_decals(&mut pal, &decal_cells, v);
        }
        if let Some(drv) = &app.driving {
            drv.apply_to_palette(gfx.renderer.skin_layout.attach_base as usize, &mut pal);
        }
        gfx.renderer.upload_skin(&gfx.queue, &pal, &cols);
        gfx.renderer.upload_morph_weights(&gfx.queue, &app.scene);
        gfx.renderer.set_char_light_centres(&app.scene);
        gfx.renderer.lights = app.scene.frame_lights();
        gfx.renderer.screen_aspect = aspect;
        let is_menu_backdrop = app.fe.as_ref().is_some_and(|fe| !fe.intro_playing && fe.flow.get_state() != menu::ST_MOVIE);
        gfx.renderer.movie_presentation = nf_view::gpu::adaptive_movie_mode(is_menu_backdrop);
        gfx.renderer.destroy_dead = dead_destroy_centres(&app.scene);
        gfx.renderer.destroy_wreck_draw = wreck_draw_centres(&app.scene);
        gfx.renderer.destroy_tint = nf_view::destroy_tints(&app.scene);
        gfx.renderer.sky_flash_mask = app.scene.sim.as_ref().map_or(0, |s| s.drones.sky.flash_mask);
        gfx.renderer.sky_spin = app.scene.sim.as_ref().map_or(0.0, |s| s.drones.sky.spin());
        gfx.renderer.set_shards(&gfx.queue, &app.scene.shard_draws());
        gfx.renderer.set_particles(&gfx.queue, &nf_view::particles_for(&app.scene, &app.cam));

        // Step FrontEnd / intro / briefing MPEG-2 movie player BEFORE building menu_hud
        // so `fe.intro_live` and `fe.backdrop_live` accurately hide the fallback plate.
        let mut movie_frame_owned: Option<(u32, u32, Vec<u8>)> = None;
        if let Some(fe) = app.fe.as_mut() {
            fe.intro_live = false;
            fe.backdrop_live = false;
            let vol = 100u32;
            if fe.intro_movie {
                let code = fe.intro_movie_code();
                let need_start = app.movie.as_ref().map_or(true, |p| p.code() != code)
                    && app.movie_failed != Some(code);
                if need_start {
                    match nf_view::movie::Player::start(&app.data_path, code, vol) {
                        Ok(p) => {
                            log::info!("movie: started intro 0x{code:08x}");
                            app.movie = Some(p);
                            app.movie_failed = None;
                        }
                        Err(e) => {
                            log::warn!("movie: intro 0x{code:08x} unavailable: {e:?}");
                            app.movie = None;
                            app.movie_failed = Some(code);
                            fe.intro_movie_finished();
                        }
                    }
                }
                if let Some(p) = app.movie.as_mut() {
                    if let Some((w, h, rgba)) = p.present() {
                        movie_frame_owned = Some((w, h, rgba.to_vec()));
                        fe.intro_live = true;
                    }
                    if p.finished() {
                        app.movie = None;
                        fe.intro_movie_finished();
                    }
                }
            } else if fe.flow.get_state() == menu::ST_MOVIE {
                let code = fe.flow.movie_code;
                let need_start = app.movie.as_ref().map_or(true, |p| p.code() != code)
                    && app.movie_failed != Some(code);
                if need_start {
                    match nf_view::movie::Player::start(&app.data_path, code, vol) {
                        Ok(p) => {
                            log::info!("movie: started briefing/cutscene 0x{code:08x}");
                            app.movie = Some(p);
                            app.movie_failed = None;
                        }
                        Err(e) => {
                            log::warn!("movie: 0x{code:08x} unavailable: {e:?}");
                            app.movie = None;
                            app.movie_failed = Some(code);
                            fe.flow.pop_state();
                        }
                    }
                }
                if let Some(p) = app.movie.as_mut() {
                    if let Some((w, h, rgba)) = p.present() {
                        movie_frame_owned = Some((w, h, rgba.to_vec()));
                    }
                    if p.finished() {
                        app.movie = None;
                        fe.flow.pop_state();
                    }
                }
            } else {
                let aux = menu::aux_movie_for(fe.page_id());
                if aux != 0 {
                    let need_start = app.movie.as_ref().map_or(true, |p| p.code() != aux || !p.is_looping())
                        && app.movie_failed != Some(aux);
                    if need_start {
                        match nf_view::movie::Player::start_looping(&app.data_path, aux, vol) {
                            Ok(p) => {
                                log::info!("movie: started looping backdrop 0x{aux:08x}");
                                app.movie = Some(p);
                                app.movie_failed = None;
                            }
                            Err(e) => {
                                log::warn!("movie: backdrop 0x{aux:08x} unavailable: {e:?}");
                                app.movie = None;
                                app.movie_failed = Some(aux);
                            }
                        }
                    }
                    if let Some(p) = app.movie.as_mut() {
                        if let Some((w, h, rgba)) = p.present() {
                            movie_frame_owned = Some((w, h, rgba.to_vec()));
                            fe.backdrop_live = true;
                        }
                    }
                } else {
                    app.movie = None;
                }
            }
        } else {
            app.movie = None;
        }
        let movie_arg = movie_frame_owned.as_ref().map(|(w, h, r)| (*w, *h, r.as_slice()));

        let snow = nf_view::snow_for(&app.scene, &app.cam);
        let suppress_menu_hud = app.fe.as_ref().is_some_and(|fe| fe.intro_live || fe.flow.get_state() == menu::ST_MOVIE);
        let menu_hud: Option<HudFrame> = if suppress_menu_hud {
            None
        } else {
            app.fe.as_mut().map(|fe| {
                let lang = app
                    .lang
                    .as_ref()
                    .or_else(|| app.scene.sim.as_ref().and_then(|s| s.lang.as_ref()));
                fe.view_h = gfx.config.height as f32;
                match &app.scene.hud {
                    Some(tables) => menu::menu_overlay_fonts(
                        fe,
                        &tables.font,
                        &tables.font_header,
                        Some(&tables.font_medium),
                        lang,
                        &app.scene.hud_textures,
                        aspect,
                    ),
                    None => overlay_for(&app.scene, aspect),
                }
            })
        };
        let hud = if suppress_menu_hud {
            HudFrame::default()
        } else if let Some(drv) = app.driving.as_mut() {
            drv.hud_frame(&app.scene, aspect)
        } else {
            shot_hud(&app.scene, aspect, menu_hud.as_ref(), false)
        };

        let render_t0 = Instant::now();
        let enc = gfx.renderer.render(
            &gfx.device,
            &gfx.queue,
            &view,
            &gfx.depth,
            vp,
            vp_rot,
            app.cam.pos.to_array(),
            app.ticks,
            &snow,
            &hud,
            movie_arg,
            None,
        );
        gfx.queue.submit([enc.finish()]);
        gfx.queue.present(frame);
        let render_ms = render_t0.elapsed().as_secs_f64() * 1000.0;
        if app.ticks % 15 == 0 {
            let fps = if dt > 0.0001 { 1.0 / dt } else { 0.0 };
            let pos = app
                .driving
                .as_ref()
                .map(|d| d.car.pos)
                .or_else(|| app.scene.sim.as_ref().map(|s| s.bond.pos))
                .unwrap_or([0.0, 0.0, 0.0]);
            let page = app.fe.as_ref().map(|f| format!("{:?}", f.page)).unwrap_or_else(|| "InGame".to_string());
            let driving_fields = app
                .driving
                .as_ref()
                .map(|d| {
                    let speed = (d.car.vel[0].powi(2) + d.car.vel[2].powi(2)).sqrt();
                    format!(
                        ",\"speed\":{speed:.2},\"mission_state\":\"{:?}\",\"ticks\":{}",
                        d.mission_state, d.ticks
                    )
                })
                .unwrap_or_default();
            emit_telemetry(format!(
                "{{\"ev\":\"frame\",\"archive\":{},\"tick\":{},\"fps\":{fps:.1},\"render_ms\":{render_ms:.2},\"page\":{},\"pos\":[{:.2},{:.2},{:.2}]{driving_fields}}}",
                json_escape(&app.archive),
                app.ticks,
                json_escape(&page),
                pos[0], pos[1], pos[2]
            ));
        }
        0
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeIsInMenu(
        _env: JNIEnv,
        _cls: JClass,
    ) -> jboolean {
        let guard = STATE.lock().unwrap();
        if guard.as_ref().is_some_and(|app| app.fe.is_some()) {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeListLevels<'a>(
        env: JNIEnv<'a>,
        _cls: JClass<'a>,
    ) -> JString<'a> {
        let guard = STATE.lock().unwrap();
        let mut list = Vec::new();
        if let Some(app) = guard.as_ref() {
            let files_dir = std::path::Path::new(&app.data_path).join("FILES");
            if let Ok(rd) = std::fs::read_dir(files_dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let lower = name.to_ascii_lowercase();
                    if lower.starts_with("0700") && lower.ends_with(".bin") {
                        list.push(name);
                    }
                }
            }
        }
        list.sort();
        if let Some(app) = guard.as_ref() {
            let drv_dir = std::path::Path::new(&app.data_path).join("DRIVING");
            for (viv, code) in [
                ("MIS01.VIV", "09000001.bin"),
                ("MIS3.VIV", "09000005.bin"),
                ("MIS4.VIV", "09000006.bin"),
                ("MIS11.VIV", "09000002.bin"),
                ("MIS13A.VIV", "09000003.bin"),
                ("MIS13B.VIV", "09000004.bin"),
                ("MIS13C.VIV", "09000007.bin"),
                ("RACE.VIV", "09000008.bin"),
            ] {
                if drv_dir.join(viv).is_file() {
                    list.push(code.to_string());
                }
            }
        }
        env.new_string(list.join(",")).unwrap()
    }

    #[no_mangle]
    pub extern "system" fn Java_com_nightfire_android_NativeBridge_nativeGetStatus<'a>(
        env: JNIEnv<'a>,
        _cls: JClass<'a>,
    ) -> JString<'a> {
        let guard = STATE.lock().unwrap();
        let text = if let Some(app) = guard.as_ref() {
            let pos = app
                .driving
                .as_ref()
                .map(|d| d.car.pos)
                .or_else(|| app.scene.sim.as_ref().map(|s| s.bond.pos))
                .unwrap_or([0.0, 0.0, 0.0]);
            let page = app
                .fe
                .as_ref()
                .map(|f| format!("{:?}", f.page))
                .unwrap_or_else(|| "InGame".to_string());
            format!(
                "{} | tick {} | {} | ({:.1}, {:.1}, {:.1})",
                app.archive, app.ticks, page, pos[0], pos[1], pos[2]
            )
        } else {
            "uninitialized".to_string()
        };
        env.new_string(text).unwrap()
    }
}
