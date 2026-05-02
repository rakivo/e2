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

use crossbeam_channel::Receiver;
use editor::*;
use editor::audioer::Audioer;
use editor::messager::{MESSAGE_DURATION_IN_MILLISECONDS};
use editor::session::{default_session_path, pretty_path, save_session};
use editor::command::{CommandContext, CommandTable, Keymap, LoadedLib, Mods};
use editor::{BLINK_START_DELAY_MS, Editor, ListerKeyDispatch, Rect, checked_reserve, gpu, prewarm_glyphs_and_print_preallocation_memory_usage};
use editor::gpu::{Gpu, INITIAL_VERTEX_BUFFER_CAPACITY};
use winit::keyboard::{Key, NamedKey};

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};

fn run_custom_layer_initialization(cx: &mut CommandContext, loaded: &LoadedLib) {
    eprintln!("[Running custom_layer_init]");

    let t0 = Instant::now();
    (loaded.init)(cx, loaded);

    eprintln!("[Ran custom_layer_init in {}us]", t0.elapsed().as_micros() as f32);
}

struct App {
    gpu:    Option<Gpu>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,

    editor: Editor,

    is_our_window_focused: bool,
    refresh_rate_millihertz: u32,

    command_table: CommandTable,
    keymap:        Keymap,

    loaded:   Option<LoadedLib>,
    lib_path: Box<Path>,
    lib_rx:   Receiver<()>,
    _watcher: RecommendedWatcher,  // must stay alive
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

            is_our_window_focused: false,
            refresh_rate_millihertz: u32::MAX,
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
                let old = self.loaded.replace(new_lib);
                drop(old);

                let commands = self.loaded.as_ref().unwrap().commands;
                self.command_table = CommandTable::from_commands(commands);
                self.keymap = Keymap::default_keymap(&mut self.command_table);

                if let Some(gpu) = &mut self.gpu {
                    let loaded = self.loaded.as_ref().unwrap();
                    let mut cx = CommandContext {
                        editor: &mut self.editor,
                        gpu,
                        command_table: &mut self.command_table,
                        event: None,
                        keymap: &mut self.keymap
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

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let win: Arc<_> = el.create_window(
            Window::default_attributes()
                .with_title("naysayer")
                .with_decorations(false)
        ).unwrap().into();

        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        let mut gpu = gpu::init(Arc::clone(&win));
        gpu.verts_mut().reserve(INITIAL_VERTEX_BUFFER_CAPACITY as _);

        let editor = &mut self.editor;
        editor.layout_panels(Rect::full(w as f32, h as f32));

        prewarm_glyphs_and_print_preallocation_memory_usage(&editor, &mut gpu);

        self.refresh_rate_millihertz = win.current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .unwrap_or(60*1000); // 60Hz

        {
            if let Some(time) = editor.session_apply_time_in_milliseconds {
                let path = pretty_path(&default_session_path());
                let message = format!("Applied session in {time}us from '{path}'");
                editor.messager.push(&message, &mut gpu);
            }
        }

        if let Some(l) = &self.loaded {
            let mut cx = CommandContext {
                editor,
                gpu: &mut gpu,
                command_table: &mut self.command_table,
                keymap: &mut self.keymap,
                event: None,
            };
            run_custom_layer_initialization(&mut cx, l);
        }

        self.gpu    = Some(gpu);
        self.window = Some(win);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.try_reload();

        let Some(win) = &self.window else { return };

        let editor = &self.editor;

        if editor.lister.open_anim > 0.0 && !editor.lister.is_open {
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
        _ = save_session(&self.editor, &default_session_path());
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = *m;
            return;
        }

        let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) else { return };

        let editor = &mut self.editor;

        let ctrl  = self.mods.state().control_key();
        let shift = self.mods.state().shift_key();
        let alt   = self.mods.state().alt_key();

        macro_rules! make_command_context {
            ($event: expr) => {
                CommandContext {
                    editor, gpu,
                    event: $event,
                    command_table: &mut self.command_table,
                    keymap: &mut self.keymap,
                }
            };
        }

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }

                if ctrl && shift && matches!(&event.logical_key, Key::Named(NamedKey::F12)) {
                    win.request_redraw();
                    println!("[Trying to Hot reload]");
                    self.force_try_reload();
                    return;
                }

                editor.hide_cursor(win);

                let mods = Mods { alt, ctrl, shift };

                let is_active_view_query = editor.active_view_id() == editor.lister_query_view;
                if is_active_view_query {
                    let result = editor.lister.lister_key(&event, mods);
                    let is_selected = matches!(result, ListerKeyDispatch::Selected);
                    match result {
                        ListerKeyDispatch::Selected | ListerKeyDispatch::Close => {
                            editor.lister.is_open = false;
                            editor.lister.is_listing_file_entries = true;

                            let panel_before_opening_lister = editor.panel_before_opening_lister.take().unwrap();
                            editor.set_active_panel(panel_before_opening_lister);

                            if is_selected {
                                let mut cx = make_command_context!(Some(&event));
                                editor_dispatch_lister_confirm(&mut cx);
                            }

                            win.request_redraw();

                            return;
                        }

                        ListerKeyDispatch::Other => {
                            editor.reset_blink();
                            win.request_redraw();
                            return;
                        }

                        ListerKeyDispatch::None => {}
                    }
                }

                if let Some(command_atom) = self.keymap.lookup(&event, mods) {
                    let Some(&command) = self.command_table.get(&command_atom) else {
                        eprintln!("[Undefined command]: {}", self.command_table.resolve(command_atom));
                        return;
                    };

                    //
                    //
                    // Commit cycle if switching to a non-cycle command
                    //
                    if [self.keymap.cycle_buffers_left_atom, self.keymap.cycle_buffers_right_atom, self.keymap.switch_buffer_atom].contains(&command_atom) {
                        editor.commit_buffer_cycle();
                    }

                    {
                        let mut cx = make_command_context!(Some(&event));
                        (command.func)(&mut cx);
                    }

                    if editor.is_lister_buffer_dirty() {
                        //
                        // Keep lister query updated
                        //

                        let query = editor.buffers[editor.lister_query_buffer].text.chars();
                        editor.lister.query.clear();
                        editor.lister.query.extend(query);
                        editor.lister.scroll = 0.0;
                        editor.lister.selected_index = if editor.lister.set_selected_index_to_1_instead_of_0 {
                            (editor.lister.items.len() > 1) as u32
                        } else {
                            0
                        };
                        editor.lister.is_query_dirty = true;
                        editor.lister.rebuild_filtered();
                        editor.lister.is_query_dirty = true; // nocheckin @DocumentThis
                    }

                    win.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if ctrl {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.01,
                    };
                    let new = (editor.scale + dy * 0.075).clamp(MIN_SCALE, MAX_SCALE);
                    rescale(editor, gpu, new);
                    win.request_redraw();
                    return;
                }

                editor.show_cursor(win);

                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * editor.line_h(),
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32,
                };

                if editor.lister.is_open() { // @Refactor
                    //
                    // Lister scroll takes priority if open and mouse is over it
                    //

                    let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
                    let (mx, my) = editor.mouse_pos;
                    if lister.contains(mx, my) {
                        let max_scroll = (
                            editor.lister.filtered.len() as f32 * editor.lister.item_h
                                + editor.lister.item_h * 2.0 - editor.lister.list_h
                        ).max(0.0);

                        editor.lister.scroll = (editor.lister.scroll - dy * 2.0).clamp(0.0, max_scroll);

                        //
                        // Update hovered index for new scroll position
                        //
                        let line_h  = editor.line_h();
                        let scale   = editor.scale;
                        let pad     = (8.0 * scale).round();
                        let input_h = (line_h + pad).round();
                        let sep     = scale.max(1.0);
                        let list_y  = lister.y + input_h + sep;
                        if my >= list_y {
                            let local_y = my - list_y + editor.lister.scroll;  // Use new scroll, not anim
                            let hovered = (local_y / editor.lister.item_h) as usize;
                            let hovered_index_before = editor.lister.hovered_index;
                            editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                                if hovered_index_before != Some(hovered as u32) {
                                    editor.audioer.play_lister_item_hover_sound();
                                }

                                Some(hovered as u32)
                            } else {
                                None
                            };
                        }

                        win.request_redraw();
                        return;
                    }
                }

                let (mx, my) = editor.mouse_pos;
                let Some(panel_id) = editor.panel_at(mx, my) else { return };
                let PanelKind::Leaf { view_id } = editor.panels[panel_id].kind else { return };

                let rect    = editor.panels[panel_id].rect;
                let buf_id  = editor.views[view_id].buffer_id;
                let total   = editor.buffers[buf_id].text.len_lines();
                let line_h  = editor.line_h();

                let old_scroll = editor.views[view_id].scroll;
                let new_scroll = (old_scroll - dy * 4.55).max(0.0);
                let max_scroll = ((total as f32 * line_h) - rect.h).max(0.0);
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

                win.request_redraw();
            }

            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                editor.show_cursor(win);

                editor.mouse_left_pressed = false;
            }

            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                editor.show_cursor(win);

                if editor_handle_left_mouse_click(editor, gpu, &mut self.command_table, &mut self.keymap) {
                    win.request_redraw();
                }

                editor.mouse_left_pressed = true;
            }

            WindowEvent::CursorMoved { position, .. } => {
                editor.show_cursor(win);

                editor.mouse_pos = (position.x as f32, position.y as f32);

                if editor.lister.is_open() { // @Refactor
                    let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
                    let (mx, my) = editor.mouse_pos;
                    let line_h  = editor.line_h();
                    let scale   = editor.scale;
                    let pad     = (8.0 * scale).round();
                    let item_h  = editor.lister.item_h;
                    let input_h = (line_h + pad).round();
                    let sep     = scale.max(1.0);
                    let list_y  = lister.y + input_h + sep;

                    if lister.contains(mx, my) && my >= list_y {
                        let local_y = my - list_y + editor.lister.scroll_anim;
                        let hovered = (local_y / item_h) as usize;
                        let hovered_index_before = editor.lister.hovered_index;
                        editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                            if hovered_index_before != Some(hovered as u32) {
                                editor.audioer.play_lister_item_hover_sound();
                            }

                            Some(hovered as u32)
                        } else {
                            None
                        };
                        win.request_redraw();
                    } else {
                        editor.lister.hovered_index = None;
                    }

                    win.request_redraw();
                }

                if editor.mouse_left_pressed {
                    if editor_handle_left_mouse_click(editor, gpu, &mut self.command_table, &mut self.keymap) {
                        win.request_redraw();
                    }
                }
            }

            WindowEvent::Resized(sz) => {
                if sz.width > 0 && sz.height > 0 {
                    gpu.win_w = sz.width  as f32;
                    gpu.win_h = sz.height as f32;
                    gpu.surface_config.width  = sz.width;
                    gpu.surface_config.height = sz.height;
                    gpu.surface.configure(&gpu.device, &gpu.surface_config);
                    editor.layout_panels(Rect::full(gpu.win_w, gpu.win_h));

                    win.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                tracy_client::frame_mark();

                let now = Instant::now();
                let dt = now.duration_since(editor.last_frame_time).as_secs_f32().min(0.05);
                editor.last_frame_time = now;
                editor.frame_count += 1;

                editor.last_is_lister_open = editor.lister.is_open();
                editor.last_messager_count = editor.messager.count;

                editor.messager.tick(dt);
                editor.messager.evict_expired(MESSAGE_DURATION_IN_MILLISECONDS);

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

                let still_animating = animate(editor, dt);

                let font_size    = editor.font_size();
                let line_h       = editor.line_h();
                let show_cursor  = editor.cursor_visible();
                let active_panel = editor.active_panel;

                let mut leaf_panels = Default::default();
                collect_leaves(editor, editor.root_panel, &mut leaf_panels);

                let mut should_request_redraw = false;
                should_request_redraw |= still_animating;

                if let Some(Some(callback)) = editor.lister.items_update_frame_update_callback.last().copied() {
                    let mut cx = make_command_context!(None);
                    should_request_redraw |= callback(&mut cx);
                }

                for &(panel_id, view_id, rect) in &leaf_panels {
                    if view_id == editor.lister_query_view {
                        // Lister buffer is drawn below
                        continue;
                    }

                    let buffer_id = editor.views[view_id].buffer_id;

                    let is_dirty = does_panel_need_rebuild(editor, view_id, buffer_id, rect, font_size, line_h);

                    should_request_redraw |= is_dirty;

                    if is_dirty {
                        rebuild_text_layout(editor, gpu, view_id, rect, font_size, line_h);
                    }

                    let show_cursor = if panel_id == active_panel {
                        //
                        // Only make cursor blink on the active panel.
                        //
                        show_cursor
                    } else {
                        true
                    };

                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu,
                        &editor.buffers[buffer_id],
                        &editor.views[view_id],
                        editor.active_view_id(),
                        editor.lister_query_view,
                        editor.scale,
                        show_cursor,
                        self.is_our_window_focused,
                        &mut editor.scratch_paren,
                    );
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                    gpu::pop_clip(gpu);
                }

                if editor.lister.is_open() {
                    //
                    // Prepare lister bg
                    //

                    let t1 = Instant::now();
                    {
                        if active_panel == editor.lister_split_panel {
                            let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
                            let t = 1.0 - (1.0 - editor.lister.open_anim).powi(4);  // Same easing as lister_rect
                            render_lister_background_frosted(gpu, lister, editor.scale, t);
                        }
                        render_lister_background(gpu, editor);
                    }
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                }

                if editor.lister.is_open() {
                    // @Cutnpaste from above

                    //
                    // Render lister query buffer
                    //

                    let view_id = editor.lister_query_view;
                    let panel_id = editor.lister_query_panel;
                    let rect = editor.panels[editor.lister_query_panel].rect;
                    let buffer_id = editor.views[view_id].buffer_id;

                    let is_dirty = does_panel_need_rebuild(editor, view_id, buffer_id, rect, font_size, line_h);

                    should_request_redraw |= is_dirty;

                    if is_dirty {
                        rebuild_text_layout(editor, gpu, view_id, rect, font_size, line_h);
                    }

                    let show_cursor = if panel_id == active_panel {
                        //
                        // Only make cursor blink on the active panel.
                        //
                        show_cursor
                    } else {
                        true
                    };

                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu,
                        &editor.buffers[buffer_id],
                        &editor.views[view_id],
                        editor.active_view_id(),
                        editor.lister_query_view,
                        editor.scale,
                        show_cursor,
                        self.is_our_window_focused,
                        &mut editor.scratch_paren,
                    );
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                    gpu::pop_clip(gpu);
                }

                for buffer in editor.buffers.values_mut() {
                    //
                    // No buffer can be dirty now!
                    //
                    buffer.is_dirty = false;
                }

                let t1 = Instant::now();
                {
                    render_lister_foreground(gpu, editor);
                    render_messager(gpu, editor);
                    draw_metrics(editor, gpu, self.refresh_rate_millihertz);
                }
                editor.render_us_acc += t1.elapsed().as_micros() as f32;

                _ = gpu::submit_frame(gpu);

                let new_cursor_visible = editor.cursor_visible();
                let blink_changed = new_cursor_visible != editor.last_cursor_visible;
                editor.last_cursor_visible = new_cursor_visible;

                should_request_redraw |= blink_changed;

                should_request_redraw |= editor.lister.is_open() != editor.last_is_lister_open;
                should_request_redraw |= editor.messager.count != editor.last_messager_count;
                should_request_redraw |= editor.messager.count != 0;
                should_request_redraw |= editor.lister.open_anim > 0.0 && !editor.lister.is_open;

                if should_request_redraw {
                    win.request_redraw();
                } else {
                    self.about_to_wait(el);
                }
            }

            WindowEvent::Focused(is_focused) => {
                self.is_our_window_focused = is_focused;
            }

            _ => {}
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
