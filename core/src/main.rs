// @Speed @Note: I really did try using custom allocators,
// but they don't seem to work when there's a dynamic library (ABI?)
// boundary involved.
//
// Worth looking into though.
//
// #[cfg(feature = "dhat")]
// #[global_allocator]
// static ALLOC: dhat::Alloc = dhat::Alloc;
//
// #[cfg(feature = "mimalloc")]
// #[global_allocator]
// static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use editor::color::Color;
use editor::*;
use editor::audioer::Audioer;
use editor::messager::{MESSAGE_DURATION_IN_MILLISECONDS};
use editor::session::{default_session_path, pretty_path, save_session};
use editor::command::{CommandContext, CommandTable, Keymap, LoadedLib, Mods};
use editor::gpu::{Gpu, INITIAL_VERTEX_BUFFER_CAPACITY, prewarm_glyphs_and_print_preallocation_memory_usage};

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};

fn run_custom_layer_initialization(cx: &mut CommandContext, loaded: &mut LoadedLib) {
    eprintln!("[Running custom_layer_init]");

    let t0 = Instant::now();
    (loaded.init)(cx, loaded);


    eprintln!("[Ran custom_layer_init in {}ms]", t0.elapsed().as_millis() as f32);

    post_custom_layer_initialization(cx.editor);
}

fn post_custom_layer_initialization(editor: &mut Editor) {
    editor.layout_panels();
    editor.recompute_buffer_display_names();
}

struct App {
    gpu:    Option<Gpu>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,

    editor: Editor,

    command_table: CommandTable,
    keymap:        Keymap,

    loaded:   Option<LoadedLib>,
    lib_path: Box<Path>,
    lib_rx:   Receiver<()>,
    _watcher: RecommendedWatcher,  // Must stay alive
}

impl App {
    fn new(audioer: Audioer, loaded: LoadedLib, lib_path: Box<Path>, lib_rx: Receiver<()>, _watcher: RecommendedWatcher) -> Self {
        let mut editor = Editor::new(audioer, EditorLoggerConfig::new());
        editor.director.kick_scan(".".as_ref(), true, true, false);

        let mut command_table = Default::default();

        App {
            editor,

            _watcher,
            lib_rx,
            loaded: Some(loaded),
            lib_path,

            gpu: None,
            window: None,

            keymap: Keymap::empty(&mut command_table),
            command_table,

            mods: Default::default(),
        }
    }

    fn try_reload(&mut self) {
        let mut triggered = self.lib_rx.try_recv().is_ok();
        while self.lib_rx.try_recv().is_ok() {  // Drain the channel
            triggered = true;
        }

        if !triggered {
            return;
        }

        self.force_try_reload();
    }

    fn force_try_reload(&mut self) {
        let result = unsafe { LoadedLib::load(&self.lib_path) };
        match result {
            Ok(new_lib) => {
                // Drop all custom data while the old dylib is still loaded so its vtable is valid
                self.editor.custom_data.transient = None;

                let old = self.loaded.replace(new_lib);
                drop(old);

                if let Some(gpu) = &mut self.gpu {
                    let loaded = self.loaded.as_mut().unwrap();
                    let mut cx = CommandContext {
                        editor: &mut self.editor,
                        gpu,
                        command_table: &mut self.command_table,
                        event_and_mods: None,
                        keymap: &mut self.keymap,
                        dont_reset_blink: true,
                    };
                    run_custom_layer_initialization(&mut cx, loaded);
                }

                if let Some(gpu) = &mut self.gpu {
                    self.editor.messager.push("[Hot reloaded custom layer]", gpu);
                }

                println!("[Hot reloaded successfully]");
            }

            Err(e) => {
                eprintln!("[Hot reload failed]: {e}");
            }
        }
    }
}

enum UserEvent {
    ExitRequested,
}

impl App {
    fn window_event_impl(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = *m;
            return;
        }

        let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) else { return };

        let editor = &mut self.editor;

        let ctrl  = self.mods.state().control_key();
        let shift = self.mods.state().shift_key();
        let alt   = self.mods.state().alt_key();

        let mods = Mods { alt, ctrl, shift };

        macro_rules! make_command_context {
            (@auto $event:expr, $dont_reset_blink:expr) => {{
                CommandContext {
                    editor, gpu,
                    event_and_mods: $event,
                    command_table: &mut self.command_table,
                    keymap: &mut self.keymap,
                    dont_reset_blink: $dont_reset_blink,
                }
            }};

            (@defer $event:expr, $dont_reset_blink:expr) => {{
                core::mem::ManuallyDrop::new(CommandContext {
                    editor, gpu,
                    event_and_mods: $event,
                    command_table: &mut self.command_table,
                    keymap: &mut self.keymap,
                    dont_reset_blink: $dont_reset_blink,
                })
            }};

            (reset )                     => { make_command_context!(@auto  None, false) };
            (reset None)                 => { make_command_context!(@auto  None, false) };
            (reset defer None)           => { make_command_context!(@defer None, false) };
            (reset defer)                => { make_command_context!(@defer None, false) };
            (reset $event:expr)          => { make_command_context!(@auto  Some(($event, mods)), false) };
            (reset defer $event:expr)    => { make_command_context!(@defer Some(($event, mods)), false) };

            ()                     => { make_command_context!(@auto  None, true) };
            (None)                 => { make_command_context!(@auto  None, true) };
            (defer None)           => { make_command_context!(@defer None, true) };
            (defer)                => { make_command_context!(@defer None, true) };
            ($event:expr)          => { make_command_context!(@auto  Some(($event, mods)), true) };
            (defer $event:expr)    => { make_command_context!(@defer Some(($event, mods)), true) };
        }

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }

                if ctrl && shift && matches!(&event.logical_key, Key::Named(NamedKey::F12)) {  // :Configuration
                    win.request_redraw();
                    println!("[Trying to Hot reload]");
                    self.force_try_reload();
                    return;
                }

                editor.hide_cursor(win);
                editor.reset_blink(); // nocheckin

                let mut should_request_redraw = false;

                {
                    let key_pressed = editor.hooks.key_pressed;
                    let (
                        custom_window_redraw_requested,
                        should_short_circuit
                    ) = key_pressed.map_or(
                        (false, false),
                        |f| f(&mut make_command_context!(reset &event))
                    );

                    should_request_redraw |= custom_window_redraw_requested;
                    if should_short_circuit {
                        if should_request_redraw {
                            win.request_redraw();
                        }
                        return;
                    }
                }

                if let Some(command_atom) = self.keymap.lookup(&event, mods) {
                    let Some(&command) = self.command_table.get(&command_atom) else {
                        eprintln!("[Undefined command]: {}", self.command_table.resolve(command_atom));
                        return;
                    };

                    {
                        // @Cutnpaste from above

                        let pre_command_execution = editor.hooks.pre_command_execution;
                        let (
                            custom_window_redraw_requested,
                            should_short_circuit
                        ) = pre_command_execution.map_or(
                            (false, false),
                            |f| f(&mut make_command_context!(reset &event), command_atom)
                        );

                        should_request_redraw |= custom_window_redraw_requested;
                        if should_short_circuit {
                            if should_request_redraw {
                                win.request_redraw();
                            }
                            return;
                        }
                    }

                    {
                        let mut cx = make_command_context!(reset &event);
                        (command.func)(&mut cx);
                    }

                    {
                        // @Cutnpaste from above

                        let post_command_execution = editor.hooks.post_command_execution;
                        let (
                            custom_window_redraw_requested,
                            should_short_circuit
                        ) = post_command_execution.map_or(
                            (false, false),
                            |f| f(&mut make_command_context!(reset &event), command_atom)
                        );

                        should_request_redraw |= custom_window_redraw_requested;
                        if should_short_circuit {
                            if should_request_redraw {
                                win.request_redraw();
                            }
                            return;
                        }
                    }

                    should_request_redraw = true; // @Redundant?
                }

                if should_request_redraw {
                    win.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if ctrl {
                    // :Configuration
                    // We might want to give the custom layer control over this?..

                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.01,
                    };
                    let new = (editor.scale + dy * 0.075).clamp(MIN_SCALE, MAX_SCALE);
                    editor::rescale(editor, new);
                    win.request_redraw();
                    return;
                }

                editor.show_cursor(win);

                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * editor.line_h(),
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32,
                };

                let mut should_request_redraw = false;

                let mouse_wheel_scrolled = editor.hooks.mouse_wheel_scrolled;
                let (
                    custom_window_redraw_requested,
                    should_short_circuit
                ) = mouse_wheel_scrolled.map_or(
                    (false, false),
                    |f| f(&mut make_command_context!(), dy)
                );

                should_request_redraw |= custom_window_redraw_requested;
                if should_short_circuit {
                    if should_request_redraw {
                        win.request_redraw();
                    }
                    return;
                }

                let (mx, my) = editor.mouse_pos;
                let Some(panel_id) = editor.panel_at(mx, my) else {
                    if should_request_redraw {
                        win.request_redraw();
                    }
                    return;
                };
                let PanelKind::Leaf { view_id } = editor.panels[panel_id].kind else {
                    if should_request_redraw {
                        win.request_redraw();
                    }
                    return;
                };

                let rect    = editor.panels[panel_id].rect;
                let buf_id  = editor.views[view_id].buffer_id;
                let total   = editor.buffers[buf_id].text.len_lines();
                let line_h  = editor.line_h();

                let old_scroll = editor.views[view_id].scroll;
                let new_scroll = (old_scroll - dy * 4.55).max(0.0);
                let max_scroll = editor.max_scroll_of(view_id);
                editor.views[view_id].scroll = new_scroll.min(max_scroll);

                // Drag cursor if it went off screen
                let (cur_line, cur_col) = editor.buffers[buf_id]
                    .cursor_line_col(&editor.views[view_id].cursor);

                let scroll     = editor.views[view_id].scroll;
                let first_vis  = (scroll / line_h) as u32;
                let last_vis = (((scroll + rect.h) / line_h) as usize)
                    .saturating_sub(1)
                    .min(total.saturating_sub(1)) as u32;

                let new_line = if cur_line < first_vis {
                    first_vis
                } else if cur_line > last_vis {
                    last_vis.min(total.saturating_sub(1) as u32) as u32
                } else {
                    cur_line  // Still visible, don't move
                };

                if new_line != cur_line {
                    let mut cursor = editor.views[view_id].cursor.clone();
                    editor.buffers[buf_id].set_cursor_line_col(new_line, cur_col, &mut cursor);

                    editor.views[view_id].cursor_target_line = new_line;
                    editor.views[view_id].cursor_target_col  = cur_col;
                    editor.views[view_id].cursor             = cursor;

                    editor.snap_cursor_to_target(view_id, new_line, cur_col, rect);
                }

                should_request_redraw |= true; // @Cleanup

                if should_request_redraw {
                    win.request_redraw();
                }
            }

            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                editor.show_cursor(win);

                editor.mouse_left_pressed = false;
            }

            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                editor.show_cursor(win);

                {
                    let mut cx = make_command_context!();
                    if editor_handle_left_mouse_click(&mut cx) {
                        win.request_redraw();
                    }
                }

                editor.mouse_left_pressed = true;
            }

            WindowEvent::CursorMoved { position, .. } => {
                editor.show_cursor(win);

                editor.mouse_pos = (position.x as f32, position.y as f32);

                let mut should_request_redraw = false;

                let mouse_moved = editor.hooks.mouse_moved;
                let (
                    custom_window_redraw_requested,
                    should_short_circuit
                ) = mouse_moved.map_or(
                    (false, false),
                    |f| f(&mut make_command_context!())
                );

                should_request_redraw |= custom_window_redraw_requested;
                if should_short_circuit {
                    if should_request_redraw {
                        win.request_redraw();
                    }
                    return;
                }

                if editor.mouse_left_pressed {
                    let mut cx = make_command_context!();
                    should_request_redraw |= editor_handle_left_mouse_click(&mut cx);
                }

                if should_request_redraw {
                    win.request_redraw();
                }
            }

            WindowEvent::Resized(sz) => {
                if sz.width > 0 && sz.height > 0 {
                    gpu.win_w = sz.width  as f32;
                    gpu.win_h = sz.height as f32;

                    editor.win_w = gpu.win_w;
                    editor.win_h = gpu.win_h;

                    gpu.surface_config.width  = sz.width;
                    gpu.surface_config.height = sz.height;
                    gpu.surface.configure(&gpu.device, &gpu.surface_config);
                    editor.layout_panels();

                    win.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                tracy_client::frame_mark();

                ShouldRequestFrameRedraw::begin_frame(&mut editor.redraw_reasons);

                let mut redraw = ShouldRequestFrameRedraw::No;

                let now = Instant::now();
                let dt = now.duration_since(editor.last_frame_time).as_secs_f32().min(0.05);

                editor.last_frame_time = now;
                editor.frame_count += 1;

                editor.last_messager_count = editor.messager.count;

                editor.messager.tick(dt);
                editor.messager.evict_expired(MESSAGE_DURATION_IN_MILLISECONDS);

                redraw |= editor.always_on_update();

                let elapsed = editor.last_fps_time.elapsed().as_secs_f32();
                if elapsed >= 0.5 {
                    editor.fps       = editor.frame_count as f32 / elapsed;
                    editor.build_us  = editor.build_us_acc       / editor.frame_count as f32;
                    editor.render_us = editor.render_us_acc      / editor.frame_count as f32;
                    editor.relex_us  = editor.relex_us_acc       / editor.frame_count as f32;

                    editor.frame_count    = 0;
                    editor.last_fps_time  = Instant::now();
                    editor.build_us_acc   = 0.0;
                    editor.relex_us_acc   = 0.0;
                    editor.render_us_acc  = 0.0;
                }

                debug_assert_eq!(gpu.clip_depth, 0, "clip stack not balanced at frame start");
                gpu.clip_depth = 0;

                if let Some(about_to_redraw_a_frame_hook) = editor.hooks.about_to_redraw_a_frame {
                    let mut cx = make_command_context!(defer);
                    redraw |= about_to_redraw_a_frame_hook(&mut cx, dt);
                }

                //
                // Ensure vertex buffer has enough capacity
                //
                {
                    let verts = gpu.verts_mut();
                    verts.clear();

                    let estimated = editor.views
                        .values()
                        .filter_map(|v| v.layout.as_ref())
                        .map(|l| l.visible_glyph_count)
                        .sum::<u32>();

                    let reserve = estimated * 6 + 4096;
                    checked_reserve!(verts, reserve as usize, "vertex buffer");
                }

                let is_cursor_visible_due_to_blinking = editor.cursor_visible();
                let active_panel = editor.active_panel;

                let mut leaf_panels = Default::default();
                collect_leaves(editor, editor.root_panel, &mut leaf_panels);

                for (_panel_id, view_id, ..) in &leaf_panels {
                    let char_index = editor.views[*view_id].cursor.char_index;
                    editor.last_cursor_position.insert(*view_id, char_index as _);
                }

                if let Some(about_to_rebuild_dirty_layouts_hook) = editor.hooks.about_to_rebuild_dirty_layouts {
                    let mut cx = make_command_context!(defer);
                    redraw |= about_to_rebuild_dirty_layouts_hook(&mut cx);
                }

                //
                //
                // Rebuild all dirty layouts
                //
                //

                for &(_panel_id, view_id, rect, _rect_including_bar) in &leaf_panels {
                    let buffer_id = editor.views[view_id].buffer_id;

                    let is_dirty = does_view_need_layout_rebuild(editor, view_id, buffer_id, rect);

                    redraw = redraw.or_if(is_dirty, "Layout rebuild", &mut editor.redraw_reasons);

                    if is_dirty {
                        rebuild_text_layout(editor, gpu, view_id, rect);
                    }
                }

                if let Some(rebuilt_all_dirty_layouts_hook) = editor.hooks.rebuilt_all_dirty_layouts {
                    let mut cx = make_command_context!(defer);
                    redraw |= rebuilt_all_dirty_layouts_hook(&mut cx);
                }

                //
                //
                // Animate
                //
                //

                redraw |= animate(editor, dt);

                if let Some(animated_all_animations_hook) = editor.hooks.animated_all_animations {
                    let mut cx = make_command_context!(defer);
                    redraw |= animated_all_animations_hook(&mut cx);
                }

                //
                //
                // Draw
                //
                //

                render_split_seams(gpu, editor, editor.root_panel, Color::hex(0x2a2a2a)); // :Configuration ?

                for &(panel_id, view_id, rect, rect_including_bar) in &leaf_panels {
                    {
                        let about_to_draw_this_panel = editor.hooks.about_to_draw_this_panel;
                        let should_skip_rendering_this_specific_panel = about_to_draw_this_panel.map_or(
                            false,
                            |f| {
                                let mut cx = make_command_context!(defer);
                                f(&mut cx, panel_id, view_id, rect)
                            }
                        );
                        if should_skip_rendering_this_specific_panel {
                            continue;
                        }
                    }

                    let r = rect_including_bar;
                    gpu::push_clip(gpu, r.x, r.y, r.w, r.h);

                    let show_cursor = if panel_id == active_panel {
                        //
                        // Only make cursor blink on the active panel.
                        //
                        is_cursor_visible_due_to_blinking
                    } else {
                        true
                    };

                    let t1 = Instant::now();
                    render_text_layout(editor, gpu, view_id, show_cursor);
                    render_panel_bar(gpu, editor, view_id);
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                    gpu::pop_clip(gpu);
                }

                if let Some(drew_all_leaf_panels_hook) = editor.hooks.drew_all_leaf_panels {
                    let mut cx = make_command_context!(defer);
                    redraw |= drew_all_leaf_panels_hook(&mut cx);
                }

                if let Some(fc) = &editor.flying_cursor {
                    let line_h = editor.line_h();
                    let font_size = editor.font_size();
                    let min_cursor_w = editor.cursor_w();
                    let space_width = gpu::get_glyph(gpu, ' ', font_size)
                        .map(|g| g.advance)
                        .unwrap_or(min_cursor_w * 4.0);
                    let min_cursor_w = min_cursor_w.max(space_width);

                    let w = min_cursor_w * 0.6;
                    let h = line_h * 0.7;
                    let x = fc.x + (w * 0.5);  // Center it
                    let y = fc.y + (line_h - h) * 0.5;
                    gpu::draw_rect(gpu, x, y, w, h, palette().cursor.with_alpha(fc.alpha * 0.6));

                    redraw = redraw.or_msg("Flying cursor", &mut editor.redraw_reasons);
                }

                let t1 = Instant::now();
                render_messager(gpu, editor);
                editor.render_us_acc += t1.elapsed().as_micros() as f32;

                for buffer in editor.buffers.values_mut() {
                    //
                    // No buffer can be dirty now!
                    //
                    buffer.is_dirty = false;
                }

                _ = gpu::submit_frame(gpu);

                let new_cursor_visible = editor.cursor_visible();
                let blink_changed = new_cursor_visible != editor.last_cursor_visible;
                editor.last_cursor_visible = new_cursor_visible;

                redraw = redraw.or_if(blink_changed, "Cursor blinking", &mut editor.redraw_reasons);
                redraw = redraw.or_if(editor.messager.count != editor.last_messager_count, "Messager animation", &mut editor.redraw_reasons);
                redraw = redraw.or_if(editor.messager.count != 0, "Messager animation", &mut editor.redraw_reasons); // nocheckin

                if let Some(inside_redraw_should_request_redraw) = editor.hooks.at_the_end_of_redraw_should_request_redraw {
                    redraw |= inside_redraw_should_request_redraw(editor);
                }

                if redraw.into() {
                    // :RedrawDebug
                    // println!("Requesting redraw: {}", redraw.display(&editor.redraw_reasons));

                    win.request_redraw();
                } else {
                    self.about_to_wait(el);
                }
            }

            WindowEvent::Focused(is_focused) => {
                self.editor.is_our_window_focused = is_focused;
            }

            _ => {}
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let win: Arc<_> = el.create_window(
            Window::default_attributes()
                .with_title("naysayer")
                .with_decorations(false)
        ).unwrap().into();

        let mut gpu = gpu::init(Arc::clone(&win));
        gpu.verts_mut().reserve(INITIAL_VERTEX_BUFFER_CAPACITY as _);

        let editor = &mut self.editor;
        editor.win_w = gpu.win_w;
        editor.win_h = gpu.win_h;

        editor.refresh_rate_millihertz = win.current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .unwrap_or(60*1000); // 60Hz

        if let Some(l) = &mut self.loaded {
            let mut cx = CommandContext {
                editor,
                gpu: &mut gpu,
                command_table: &mut self.command_table,
                keymap: &mut self.keymap,
                event_and_mods: None,
                dont_reset_blink: true,
            };
            run_custom_layer_initialization(&mut cx, l);
        }

        post_custom_layer_initialization(editor);

        prewarm_glyphs_and_print_preallocation_memory_usage(&editor, &mut gpu);

        self.gpu    = Some(gpu);
        self.window = Some(win);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.try_reload();

        let editor = &mut self.editor;
        let always_on_should_request_redraw = editor.always_on_update().is_yes();

        let Some(win) = &self.window else { return };

        if always_on_should_request_redraw {
            win.request_redraw();
        }

        if editor.hooks.inside_about_to_wait_should_request_redraw.map_or(
            Default::default(),
            |f| f(editor)
        ).is_yes() {
            win.request_redraw();
            return;
        }

        let since_input = editor.last_input_time.elapsed().as_millis();

        if since_input < BLINK_START_DELAY_MS {
            //
            // Waiting to start blinking - wake up when delay expires
            //
            let ms_until = BLINK_START_DELAY_MS - since_input;
            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(ms_until as u64)
            ));

        } else if since_input > BLINK_STOP_IDLE_MS {
            //
            // Idle too long - just wait for input
            //
            el.set_control_flow(ControlFlow::Wait);

        } else {
            //
            // Actively blinking - wake up at next blink transition
            //
            let elapsed = editor.blink_epoch.elapsed().as_millis();
            let cycle   = BLINK_ON_MS + BLINK_OFF_MS;
            let phase   = elapsed % cycle;
            let ms_until = if phase < BLINK_ON_MS {
                BLINK_ON_MS - phase
            } else {
                cycle - phase
            };

            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(ms_until as u64)
            ));

            win.request_redraw();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ExitRequested => {
                el.exit();
            }
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        if let Some(hook) = self.editor.hooks.exiting {
            hook(&mut self.editor);
        }

        _ = save_session(&self.editor, &default_session_path());
    }

    fn window_event(&mut self, el: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        self.window_event_impl(el, window_id, event);
        if self.editor.window_event_finish() {
            if let Some(win) = &self.window {
                win.request_redraw();
            }
        }
    }
}

fn main() {
    let _client = tracy_client::Client::start();

    // @Note: We want to start Audio initialization as soon as possible,
    // because audio servers tend to be VERY slow when trying to initialize a connection,
    // very sad ...
    let audioer = Audioer::spawn();

    let lib_filename = libloading::library_filename("custom");
    let path_to_this_crate      = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path_to_dir_with_target = path_to_this_crate.parent().unwrap_or(path_to_this_crate);

    let profile = std::env::args()
        .skip_while(|a| a != "--profile")
        .nth(1)
        .unwrap_or_else(|| env!("CARGO_PROFILE").into());

    let lib_dir_path = path_to_dir_with_target.join("target").join(profile);
    let lib_path = lib_dir_path.join(&lib_filename);

    let (tx, lib_rx) = crossbeam_channel::unbounded();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let relevant = event.paths.iter().any(|p| p.file_name() == Some(lib_filename.as_os_str()));
            if !relevant { return; }
            use notify::EventKind::*;
            match event.kind {
                Create(_) | Modify(_) => { _ = tx.send(()); }
                _ => {}
            }
        }
    }).unwrap();

    println!("[Watching '{lib_path}' for automatic Hot reload]", lib_path = pretty_path(&lib_path));
    watcher.watch(&lib_dir_path, RecursiveMode::NonRecursive).unwrap();

    let loaded = unsafe {
        LoadedLib::load(&lib_path)
            .unwrap_or_else(|e| panic!("Failed to load `{}`: {e}", lib_path.display()))
    };

    let Ok(el) = EventLoop::<UserEvent>::with_user_event().build() else { return };
    el.set_control_flow(ControlFlow::Wait);

    ctrlc::set_handler({
        let proxy = el.create_proxy();
        move || _ = proxy.send_event(UserEvent::ExitRequested)
    }).unwrap();

    let mut app = App::new(audioer, loaded, lib_path.into(), lib_rx, watcher);
    _ = el.run_app(&mut app);
}
