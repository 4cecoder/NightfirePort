//! Safe, idiomatic Rust macOS harness engine wrapping `nf-view`, `nf-game`,
//! `nf-audio`, and headless `wgpu` Metal rendering.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use glam::Vec3;
use nf_view::driving::{driving_viv_for_level, load_driving_scene, DrivingState};
use nf_view::gpu::{self, Renderer};
use nf_view::menu::{self, Action as MenuAction, FrontEnd};
use nf_view::scene::{Scene, ViewOpts};
use nf_view::{
    cam_axes, dead_destroy_centres, device, gpu_instance_headless, load_front_end,
    load_with_carry, overlay_for, shot_hud, wreck_draw_centres, Camera, GAME_FOV,
};

pub const BTN_FIRE: i32 = 1 << 0;
pub const BTN_AIM: i32 = 1 << 1;
pub const BTN_RELOAD: i32 = 1 << 2;
pub const BTN_JUMP: i32 = 1 << 3;
pub const BTN_CROUCH: i32 = 1 << 4;
pub const BTN_USE: i32 = 1 << 5;
pub const BTN_WEAP_NEXT: i32 = 1 << 6;
pub const BTN_WEAP_PREV: i32 = 1 << 7;
pub const BTN_PAUSE: i32 = 1 << 8;

/// Normalized input state mirrored from `nf-android`'s `NativeBridge`.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputState {
    pub move_x: f32,
    pub move_y: f32,
    pub look_dx: f32,
    pub look_dy: f32,
    pub buttons: i32,
}

/// Snapshot returned after stepping one frame.
#[derive(Clone, Debug)]
pub struct FrameTelemetry {
    pub archive: String,
    pub tick: u32,
    pub mode: String,
    pub pos: [f32; 3],
    pub speed: Option<f32>,
}

/// Pixel stats returned after rendering a Metal frame to PNG.
#[derive(Clone, Copy, Debug)]
pub struct RenderStats {
    pub width: u32,
    pub height: u32,
    pub mean_luma: f64,
    pub std_luma: f64,
}

struct MetalContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    format: wgpu::TextureFormat,
}

impl MetalContext {
    fn new(scene: &Scene) -> Result<Self> {
        let instance = gpu_instance_headless();
        let (_adapter, device, queue) = pollster::block_on(device(&instance, None))?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let renderer = Renderer::new(&device, &queue, format, scene, false, false);
        Ok(Self {
            device,
            queue,
            renderer,
            format,
        })
    }

    fn rebuild(&mut self, scene: &Scene) {
        self.renderer = Renderer::new(&self.device, &self.queue, self.format, scene, false, false);
    }
}

/// Stateful macOS harness engine supporting FrontEnd, On-Foot, and Driving levels.
pub struct MacEngine {
    pub data_path: PathBuf,
    pub archive: String,
    pub scene: Scene,
    pub driving: Option<DrivingState>,
    pub cam: Camera,
    pub fe: Option<FrontEnd>,
    pub ticks: u32,
    pub input: InputState,
    prev_buttons: i32,
    metal: Option<MetalContext>,
}

impl MacEngine {
    /// Initializes the macOS engine against an ISO image or carved asset directory.
    pub fn init(data_path: impl AsRef<Path>) -> Result<Self> {
        let data_path = data_path.as_ref().to_path_buf();
        if !data_path.exists() {
            return Err(anyhow!("data path does not exist: {}", data_path.display()));
        }
        let data_str = data_path.to_string_lossy().to_string();
        let (scene, driving, fe, cam, archive) = load_scene_for(&data_str, "07000048.bin")?;
        Ok(Self {
            data_path,
            archive,
            scene,
            driving,
            cam,
            fe,
            ticks: 0,
            input: InputState::default(),
            prev_buttons: 0,
            metal: None,
        })
    }

    /// Loads an on-foot archive (`0700xxxx.bin`), front-end (`07000048.bin`),
    /// or driving mission (`09000001.bin`..`09000008.bin` / `MIS01`..`RACE`).
    pub fn load_level(&mut self, archive: &str) -> Result<()> {
        let data_str = self.data_path.to_string_lossy().to_string();
        let (scene, driving, fe, cam, resolved) = load_scene_for(&data_str, archive)?;
        self.scene = scene;
        self.driving = driving;
        self.fe = fe;
        self.cam = cam;
        self.archive = resolved;
        self.ticks = 0;
        self.prev_buttons = 0;
        if let Some(m) = self.metal.as_mut() {
            m.rebuild(&self.scene);
        }
        Ok(())
    }

    /// Updates the current virtual controller / keyboard input state.
    pub fn set_input(&mut self, input: InputState) {
        self.input = input;
    }

    /// Steps the active simulation (FrontEnd, On-Foot, or Driving) by `dt_secs`.
    pub fn step(&mut self, dt_secs: f32) -> FrameTelemetry {
        let dt = dt_secs.clamp(1.0 / 240.0, 1.0 / 15.0);
        let n_ticks = ((dt * 60.0).round() as u32).clamp(1, 4);
        let just = self.input.buttons & !self.prev_buttons;
        self.prev_buttons = self.input.buttons;

        if let Some(fe) = self.fe.as_mut() {
            if (just & BTN_FIRE) != 0 || (just & BTN_USE) != 0 {
                fe.action(MenuAction::Confirm);
            } else if (just & BTN_CROUCH) != 0 {
                fe.action(MenuAction::Back);
            }
            for _ in 0..n_ticks {
                fe.tick();
                self.ticks = self.ticks.wrapping_add(1);
            }
        } else if let Some(drv) = self.driving.as_mut() {
            let throttle = self.input.move_y.max(0.0).clamp(0.0, 1.0);
            let brake = (-self.input.move_y).max(0.0).clamp(0.0, 1.0);
            let steer = self.input.move_x.clamp(-1.0, 1.0);
            let handbrake = (self.input.buttons & BTN_JUMP) != 0;
            for _ in 0..n_ticks {
                drv.step(throttle, brake, steer, handbrake);
                self.ticks = self.ticks.wrapping_add(1);
            }
            let (p, yaw, pitch) = drv.chase_camera();
            self.cam = Camera {
                pos: Vec3::new(p[0], p[1], p[2]),
                yaw,
                pitch,
                fov: GAME_FOV,
            };
        } else if let Some(sim) = self.scene.sim.as_mut() {
            let b = self.input.buttons;
            let weap_cycle = if (just & BTN_WEAP_NEXT) != 0 {
                1
            } else if (just & BTN_WEAP_PREV) != 0 {
                -1
            } else {
                0
            };
            let mut turn = self.input.look_dx;
            let mut look = self.input.look_dy;
            let mut jump = (just & BTN_JUMP) != 0;
            let mut crouch = (just & BTN_CROUCH) != 0;
            let mut reload = (just & BTN_RELOAD) != 0;
            let mut use_key = (just & BTN_USE) != 0;
            let fire = (b & BTN_FIRE) != 0;

            for _ in 0..n_ticks {
                let pi = nf_game::player::Input {
                    forward: self.input.move_y.clamp(-1.0, 1.0),
                    strafe: -self.input.move_x.clamp(-1.0, 1.0),
                    lift: if (b & BTN_JUMP) != 0 { 1.0 } else { 0.0 }
                        - if (b & BTN_CROUCH) != 0 { 1.0 } else { 0.0 },
                    stabilise: false,
                    turn,
                    look,
                    jump,
                    crouch,
                    crouch_hold: false,
                    aiming: (b & BTN_AIM) != 0,
                    weapon_cycle: weap_cycle,
                    gadget_cycle: 0,
                    zoom_cycle: 0,
                    zoom_axis: 0.0,
                    switch_to: None,
                    fire,
                    zoom: (b & BTN_AIM) != 0,
                    scope: (b & BTN_AIM) != 0,
                    mode_switch: false,
                    goggles_action: false,
                    keypad_confirm: false,
                    keypad_back: false,
                    keypad_nav: 0,
                };
                sim.step_bond(pi, fire, reload, use_key);
                turn = 0.0;
                look = 0.0;
                jump = false;
                crouch = false;
                reload = false;
                use_key = false;
                self.ticks = self.ticks.wrapping_add(1);
            }
            self.cam.pos = Vec3::new(
                sim.bond.pos[0],
                sim.bond.pos[1] + sim.bond.eye - sim.bond.crouch_drop,
                sim.bond.pos[2],
            );
            self.cam.yaw = sim.bond.yaw;
            self.cam.pitch = sim.bond.pitch;
        } else {
            self.ticks = self.ticks.wrapping_add(n_ticks);
        }

        self.telemetry()
    }

    /// Renders the current frame offscreen via wgpu Metal and writes a PNG to `out_path`.
    pub fn render_png(&mut self, width: u32, height: u32, out_path: impl AsRef<Path>) -> Result<RenderStats> {
        let w = width.max(64);
        let h = height.max(64);
        if self.metal.is_none() {
            self.metal = Some(MetalContext::new(&self.scene)?);
        }
        let m = self.metal.as_mut().unwrap();
        let aspect = w as f32 / h as f32;
        let (vp, vp_rot) = self.cam.view_proj(aspect);
        let (opts_right, opts_up) = cam_axes(&self.cam);
        let opts = ViewOpts {
            reinforcements: false,
            firing: (self.input.buttons & BTN_FIRE) != 0,
            right: opts_right,
            up: opts_up,
        };

        m.renderer.destroy_dead = dead_destroy_centres(&self.scene);
        m.renderer.destroy_wreck_draw = wreck_draw_centres(&self.scene);
        m.renderer.sky_flash_mask = self.scene.sim.as_ref().map_or(0, |s| s.drones.sky.flash_mask);
        m.renderer.sky_spin = self.scene.sim.as_ref().map_or(0.0, |s| s.drones.sky.spin());
        m.renderer.update_skin(&m.queue, &self.scene, None, self.ticks as f32, opts, None);

        if let Some(drv) = self.driving.as_ref() {
            let mut pal = self
                .scene
                .skin_palette(&m.renderer.skin_layout, None, self.ticks as f32, opts, None)
                .0;
            drv.apply_to_palette(m.renderer.skin_layout.attach_base as usize, &mut pal);
            m.renderer.upload_palette(&m.queue, &pal);
        }

        m.renderer.set_char_light_centres(&self.scene);
        m.renderer.lights = self.scene.frame_lights();
        m.renderer.screen_aspect = aspect;

        let target = m.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nf-macos-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: m.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let depth = gpu::depth_view(&m.device, w, h);

        let snow = nf_view::snow_for(&self.scene, &self.cam);
        let menu_hud = self.fe.as_mut().map(|fe| {
            let lang = self.scene.sim.as_ref().and_then(|s| s.lang.as_ref());
            fe.view_h = h as f32;
            match &self.scene.hud {
                Some(tables) => menu::menu_overlay_fonts(
                    fe,
                    &tables.font,
                    &tables.font_header,
                    Some(&tables.font_medium),
                    lang,
                    &self.scene.hud_textures,
                    aspect,
                ),
                None => overlay_for(&self.scene, aspect),
            }
        });
        let mut hud = shot_hud(&self.scene, aspect, menu_hud.as_ref(), false);
        if let Some(drv) = self.driving.as_ref() {
            hud.append(&nf_view::driving::driving_hud(drv, aspect));
        }

        let mut enc = m.renderer.render(
            &m.device,
            &m.queue,
            &view,
            &depth,
            vp,
            vp_rot,
            self.cam.pos.to_array(),
            self.ticks,
            &snow,
            &hud,
            None,
            None,
        );

        let row_bytes = (4 * w).next_multiple_of(256);
        let buf = m.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nf-macos-readback"),
            size: (row_bytes * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        m.queue.submit([enc.finish()]);

        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = m.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv()
            .context("readback channel closed")?
            .map_err(|e| anyhow!("buffer map failed: {e:?}"))?;

        let mapped = slice
            .get_mapped_range()
            .map_err(|e| anyhow!("get_mapped_range failed: {e:?}"))?;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            let start = y * row_bytes as usize;
            rgba.extend_from_slice(&mapped[start..start + (w as usize * 4)]);
        }
        drop(mapped);
        buf.unmap();

        let (mean_luma, std_luma) = luma_stats(&rgba, w, h);
        let png_bytes = nf_formats::png::encode_rgba(w, h, &rgba);
        let out_path = out_path.as_ref();
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(out_path, png_bytes)?;

        Ok(RenderStats {
            width: w,
            height: h,
            mean_luma,
            std_luma,
        })
    }

    /// Returns whether the engine is currently in the FrontEnd menu.
    pub fn is_in_menu(&self) -> bool {
        self.fe.is_some()
    }

    /// Lists available on-foot and driving level archives.
    pub fn list_levels(&self) -> Vec<String> {
        let mut out = vec![
            "07000048.bin",
            "07000001.bin",
            "07000002.bin",
            "07000003.bin",
            "07000005.bin",
            "07000006.bin",
            "07000007.bin",
            "07000008.bin",
            "07000009.bin",
            "0700000a.bin",
            "0700000b.bin",
            "0700000c.bin",
            "0700000d.bin",
            "07000011.bin",
            "07000012.bin",
            "07000014.bin",
            "07000015.bin",
            "07000016.bin",
            "0700001b.bin",
            "07000023.bin",
            "09000001.bin",
            "09000002.bin",
            "09000003.bin",
            "09000004.bin",
            "09000005.bin",
            "09000006.bin",
            "09000007.bin",
            "09000008.bin",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        out.sort();
        out
    }

    /// Returns the live telemetry snapshot.
    pub fn telemetry(&self) -> FrameTelemetry {
        let pos = self
            .driving
            .as_ref()
            .map(|d| d.car.pos)
            .or_else(|| self.scene.sim.as_ref().map(|s| s.bond.pos))
            .unwrap_or([0.0, 0.0, 0.0]);
        let mode = self
            .fe
            .as_ref()
            .map(|f| format!("{:?}", f.page))
            .unwrap_or_else(|| {
                if self.driving.is_some() {
                    "Driving".to_string()
                } else {
                    "InGame".to_string()
                }
            });
        let speed = self
            .driving
            .as_ref()
            .map(|d| (d.car.vel[0].powi(2) + d.car.vel[2].powi(2)).sqrt());
        FrameTelemetry {
            archive: self.archive.clone(),
            tick: self.ticks,
            mode,
            pos,
            speed,
        }
    }

    /// Formats a human-readable status line matching `nf-android`'s `nativeGetStatus`.
    pub fn status_line(&self) -> String {
        let t = self.telemetry();
        format!(
            "{} | tick {} | {} | ({:.1}, {:.1}, {:.1})",
            t.archive, t.tick, t.mode, t.pos[0], t.pos[1], t.pos[2]
        )
    }
}

fn resolve_driving_viv(archive: &str) -> Option<&'static str> {
    let trimmed = archive.trim();
    if let Ok(code) = u32::from_str_radix(trimmed.trim_end_matches(".bin"), 16) {
        if let Some(viv) = driving_viv_for_level(code) {
            return Some(viv);
        }
    }
    match trimmed.to_ascii_uppercase().trim_end_matches(".VIV") {
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

fn load_scene_for(
    data_path: &str,
    archive: &str,
) -> Result<(Scene, Option<DrivingState>, Option<FrontEnd>, Camera, String)> {
    if let Some(viv) = resolve_driving_viv(archive) {
        let (scene, drv) = load_driving_scene(data_path, viv).map_err(|e| anyhow!(e))?;
        let (p, yaw, pitch) = drv.chase_camera();
        let cam = Camera {
            pos: Vec3::new(p[0], p[1], p[2]),
            yaw,
            pitch,
            fov: GAME_FOV,
        };
        return Ok((scene, Some(drv), None, cam, viv.trim_end_matches(".VIV").to_string()));
    }

    if archive.eq_ignore_ascii_case("07000048.bin") {
        let scene = load_front_end(data_path, archive, true, None, false, 1, 0)?;
        let mut fe = FrontEnd::new();
        fe.unlock_all = true;
        fe.page = menu::Page::Main;
        let cam = Camera {
            pos: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov: GAME_FOV,
        };
        return Ok((scene, None, Some(fe), cam, archive.to_string()));
    }

    let scene = load_with_carry(data_path, archive, false, None)?;
    let (pos, yaw) = scene
        .sim
        .as_ref()
        .map(|s| {
            (
                [
                    s.bond.pos[0],
                    s.bond.pos[1] + s.bond.eye - s.bond.crouch_drop,
                    s.bond.pos[2],
                ],
                s.bond.yaw,
            )
        })
        .unwrap_or(([0.0, 1.6, 0.0], 0.0));
    let cam = Camera {
        pos: Vec3::new(pos[0], pos[1], pos[2]),
        yaw,
        pitch: 0.0,
        fov: GAME_FOV,
    };
    Ok((scene, None, None, cam, archive.to_string()))
}

fn luma_stats(rgba: &[u8], w: u32, h: u32) -> (f64, f64) {
    let mut lumas = Vec::new();
    for y in (0..h as usize).step_by(8) {
        for x in (0..w as usize).step_by(8) {
            let o = (y * w as usize + x) * 4;
            let l = 0.299 * rgba[o] as f64 + 0.587 * rgba[o + 1] as f64 + 0.114 * rgba[o + 2] as f64;
            lumas.push(l);
        }
    }
    let n = lumas.len().max(1) as f64;
    let mean = lumas.iter().sum::<f64>() / n;
    let var = lumas.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_driving_codes_and_short_names() {
        assert_eq!(resolve_driving_viv("09000001.bin"), Some("MIS01"));
        assert_eq!(resolve_driving_viv("MIS01"), Some("MIS01"));
        assert_eq!(resolve_driving_viv("mis13a.viv"), Some("MIS13A"));
        assert_eq!(resolve_driving_viv("07000005.bin"), None);
    }

    #[test]
    fn computes_luma_stats_cleanly() {
        let mut rgba = vec![0u8; 64 * 64 * 4];
        for i in 0..(64 * 64) {
            let v = if i % 2 == 0 { 200 } else { 40 };
            rgba[i * 4] = v;
            rgba[i * 4 + 1] = v;
            rgba[i * 4 + 2] = v;
            rgba[i * 4 + 3] = 255;
        }
        let (mean, std) = luma_stats(&rgba, 64, 64);
        assert!((mean - 200.0).abs() < 1e-6);
        assert!(std.abs() < 1e-6);
    }
}
