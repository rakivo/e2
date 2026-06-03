// @Important @Note: Must match the allocator `custom` uses

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use editor::color::Color;
use editor::*;
use editor::audioer::Audioer;
use editor::messager::{MESSAGE_DURATION_IN_MILLISECONDS};
use editor::session::{default_session_path, pretty_path, save_session};
use editor::command::{CommandAtom, CommandContext, CommandTable, KeyInput, Keymap, LoadedLib, LogicalKey, Mods, NamedKey, PhysicalKey};
use editor::gpu::{Gpu, INITIAL_VERTEX_BUFFER_CAPACITY, prewarm_glyphs_and_print_preallocation_memory_usage, wait_for_atlas_upload};

use std::path::Path;
use std::time::{Duration, Instant};

use sdl2::event::Event;
use sdl2::event::WindowEvent as SdlWindowEvent;
use sdl2::mouse::MouseButton;
use crossbeam_channel::Receiver;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

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

macro_rules! run_hook {
    ($hook:expr, $cx:expr) => {{
        let (redraw, short_circuit) = $hook.map_or((false, false), |f| f($cx));
        (redraw, short_circuit)
    }};

    ($hook:expr, $cx:expr, $atom:expr) => {{
        let (redraw, short_circuit) = $hook.map_or((false, false), |f| f($cx, $atom));
        (redraw, short_circuit)
    }};
}

macro_rules! define_command_context_macro {
    (
        editor = $editor:expr,
        gpu = $gpu:expr,
        command_table = $command_table:expr,
        keymap = $keymap:expr
    ) => {
        macro_rules! make_command_context {
            (@auto $event:expr, $dont_reset_blink:expr, $text:expr) => {{
                CommandContext {
                    editor: $editor,
                    gpu: $gpu,
                    key_input: $event,
                    command_table: $command_table,
                    keymap: $keymap,
                    dont_reset_blink: $dont_reset_blink,
                    text_input_char: $text,
                }
            }};

            (@defer $event:expr, $dont_reset_blink:expr, $text:expr) => {{
                core::mem::ManuallyDrop::new(CommandContext {
                    editor: $editor,
                    gpu: $gpu,
                    key_input: $event,
                    command_table: $command_table,
                    keymap: $keymap,
                    dont_reset_blink: $dont_reset_blink,
                    text_input_char: $text,
                })
            }};

            (reset)                            => { make_command_context!(@auto  None,         false, None)       };
            (reset None)                       => { make_command_context!(@auto  None,         false, None)       };
            (reset defer None)                 => { make_command_context!(@defer None,         false, None)       };
            (reset defer)                      => { make_command_context!(@defer None,         false, None)       };
            (reset $event:expr)                => { make_command_context!(@auto  Some($event), false, None)       };
            (reset defer $event:expr)          => { make_command_context!(@defer Some($event), false, None)       };
            (opt reset $event:expr)            => { make_command_context!(@auto  $event,       false, None)       };
            (opt reset defer $event:expr)      => { make_command_context!(@defer $event,       false, None)       };

            ()                                 => { make_command_context!(@auto  None,         true,  None)       };
            (None)                             => { make_command_context!(@auto  None,         true,  None)       };
            (defer None)                       => { make_command_context!(@defer None,         true,  None)       };
            (defer)                            => { make_command_context!(@defer None,         true,  None)       };
            ($event:expr)                      => { make_command_context!(@auto  Some($event), true,  None)       };
            (defer $event:expr)                => { make_command_context!(@defer Some($event), true,  None)       };
            (opt $event:expr)                  => { make_command_context!(@auto  $event,       true,  None)       };
            (opt defer $event:expr)            => { make_command_context!(@defer $event,       true,  None)       };

            // text only
            (text $c:expr)                     => { make_command_context!(@auto  None,         true,  Some($c))   };
            (reset text $c:expr)               => { make_command_context!(@auto  None,         false, Some($c))   };

            // event + text
            (reset $event:expr, text $c:expr)         => { make_command_context!(@auto  Some($event), false, Some($c)) };
            (reset $event:expr, opt text $c:expr)     => { make_command_context!(@auto  Some($event), false, $c)       };
            ($event:expr, text $c:expr)               => { make_command_context!(@auto  Some($event), true,  Some($c)) };
            ($event:expr, opt text $c:expr)           => { make_command_context!(@auto  Some($event), true,  $c)       };
            ($event:expr, opt text $c:expr)           => { make_command_context!(@auto  Some($event), true,  $c)       };
            (opt $event:expr, text $c:expr)           => { make_command_context!(@auto  $event,       true,  Some($c)) };
            (opt $event:expr, opt text $c:expr)       => { make_command_context!(@auto  $event,       true,  $c)       };
            (opt reset $event:expr, text $c:expr)     => { make_command_context!(@auto  $event,       false, Some($c)) };
            (opt reset $event:expr, opt text $c:expr) => { make_command_context!(@auto  $event,       false, $c)       };

            // defer + event + text
            (reset defer $event:expr, text $c:expr)     => { make_command_context!(@defer Some($event), false, Some($c)) };
            (reset defer $event:expr, opt text $c:expr) => { make_command_context!(@defer Some($event), false, $c)       };
            (defer $event:expr, text $c:expr)           => { make_command_context!(@defer Some($event), true,  Some($c)) };
            (defer $event:expr, opt text $c:expr)       => { make_command_context!(@defer Some($event), true,  $c)       };
        }
    };
}

struct App {
    editor: Editor,
    gpu:    Option<Gpu>,

    window: Option<sdl2::video::Window>,

    mods:   Mods,

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
                //
                // Drop all custom data while the old dylib is still loaded so its vtable is valid
                //
                self.editor.custom_data.transient = None;

                let old = self.loaded.replace(new_lib);
                drop(old);

                if let Some(gpu) = &mut self.gpu {
                    let loaded = self.loaded.as_mut().unwrap();
                    let mut cx = CommandContext {
                        editor: &mut self.editor,
                        gpu,
                        command_table: &mut self.command_table,
                        key_input: None,
                        keymap: &mut self.keymap,
                        dont_reset_blink: true,
                        text_input_char: None
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

fn main() {
    // @Note: This crashes for some reason?
    // #[cfg(feature = "dhat")]
    // let _profiler = dhat::Profiler::new_heap();

    let _client = tracy::Client::start();

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

    let app = App::new(audioer, loaded, lib_path.into(), lib_rx, watcher);
    run(app);
}

fn run(mut app: App) {
    sdl2::hint::set("SDL_MAC_OPTION_AS_ALT", "1");  // :Configuration

    let sdl   = sdl2::init().unwrap();
    let video = sdl.video().unwrap();

    let window = video.window("e2", 1280, 720)
        .borderless()
        .resizable()
        .vulkan()
        .build()
        .unwrap();

    let refresh_rate_millihertz = {
        let di = window.display_index().unwrap_or(0);
        let mut mode = sdl2::sys::SDL_DisplayMode {
            format: 0, w: 0, h: 0, refresh_rate: 0,
            driverdata: std::ptr::null_mut(),
        };
        unsafe { sdl2::sys::SDL_GetCurrentDisplayMode(di, &mut mode); }
        if mode.refresh_rate > 0 { mode.refresh_rate as u32 * 1000 } else { 60_000 }
    };

    let (win_w, win_h) = window.size();
    let mut gpu = Gpu::new(
        win_w, win_h,
        window.display_handle().unwrap(),
        window.window_handle().unwrap(),
    );
    gpu.verts_mut().reserve(INITIAL_VERTEX_BUFFER_CAPACITY as _);

    {
        let editor = &mut app.editor;
        editor.win_w = gpu.win_w;
        editor.win_h = gpu.win_h;
        editor.ui.win_w = gpu.win_w;
        editor.ui.win_h = gpu.win_h;
        editor.refresh_rate_millihertz = refresh_rate_millihertz;

        if let Some(l) = &mut app.loaded {
            let mut cx = CommandContext {
                editor,
                gpu: &mut gpu,
                command_table: &mut app.command_table,
                keymap: &mut app.keymap,
                key_input: None,
                dont_reset_blink: true,
                text_input_char: None
            };
            run_custom_layer_initialization(&mut cx, l);
        }

        post_custom_layer_initialization(editor);

        let t0 = Instant::now();
        prewarm_glyphs_and_print_preallocation_memory_usage(editor, &mut gpu);
        println!("[Prewarmed glyphs in {}ms]", t0.elapsed().as_millis());
    }

    app.window = Some(window);
    app.gpu    = Some(gpu);

    let refresh_rate_mhz = app.editor.refresh_rate_millihertz.max(60_000);
    let target_frame_time = Duration::from_micros(1_000_000_000 / refresh_rate_mhz as u64);
    let target_frame_time_ms = target_frame_time.as_millis() as u32;

    let basic_character_atom = app.keymap.basic_character_atom;

    let mut event_pump = sdl.event_pump().unwrap();
    let mut early_event: Option<sdl2::event::Event> = None;

    let mut last_input_time = Instant::now();
    let mut frame_start = Instant::now();

    let mut renderer_requested_redraw = false;

    'main: loop {
        let now = Instant::now();
        let dt = now.duration_since(frame_start).as_secs_f32().min(0.05);
        frame_start = now;

        //
        // Compute sleep timeout
        //
        app.try_reload();

        let editor = &mut app.editor;
        editor.frame_time_in_seconds = dt;

        //
        // Tick messager
        //
        let messager_needs_redraw = {
            let now = Instant::now();
            let dt_for_messager = now.duration_since(editor.last_frame_time).as_secs_f32().min(0.05);
            editor.messager.tick(dt_for_messager);
            editor.messager.evict_expired(MESSAGE_DURATION_IN_MILLISECONDS);

            let messager_needs_redraw = editor.messager.count != editor.last_messager_count || editor.messager.count != 0;
            editor.last_messager_count = editor.messager.count;

            messager_needs_redraw
        };

        let always_on = editor.always_on_update().is_yes()
            || editor.hooks.inside_about_to_wait_should_request_redraw.map_or(false, |f| f(editor).is_yes())
            || renderer_requested_redraw
            || messager_needs_redraw;

        let since_input = last_input_time.elapsed().as_millis();

        let (timeout_ms, blink_redraw): (u32, bool) = if always_on {
            (0, true)
        } else if since_input < BLINK_START_DELAY_MS {
            //
            // Waiting to start blinking - sleep until delay expires, no redraw
            //
            let ms_until = BLINK_START_DELAY_MS - since_input;
            (ms_until as u32, false)
        } else if since_input > BLINK_STOP_IDLE_MS {
            //
            // Idle - wait indefinitely, no redraw
            //
            (u32::MAX, false)
        } else {
            //
            // Actively blinking - wake every X ms and redraw for cursor animation
            //
            (target_frame_time_ms, true)
        };

        //
        // Block or poll for events
        //
        let first = match timeout_ms {
            0         => event_pump.poll_event(),
            u32::MAX  => Some(event_pump.wait_event()),
            ms        => event_pump.wait_event_timeout(ms),
        };

        let mut should_redraw = blink_redraw || renderer_requested_redraw;
        let mut input_this_frame = false;

        let events = early_event.take().into_iter().chain(first.into_iter()).chain(event_pump.poll_iter());
        for event in events {
            //
            // Keep mods in sync from any key event
            //
            if let Event::KeyDown { keymod, .. } | Event::KeyUp { keymod, .. } = event {
                app.mods = mods_from_sdl(keymod);
                app.editor.modifiers = app.mods; // @Cleanup nocheckin
            }

            let gpu    = app.gpu.as_mut().unwrap();
            let editor = &mut app.editor;

            define_command_context_macro!(
                editor = editor,
                gpu = gpu,
                command_table = &mut app.command_table,
                keymap = &mut app.keymap
            );

            match event {
                Event::Quit { .. } => break 'main,

                Event::Window { win_event: SdlWindowEvent::Resized(w, h), .. } => {
                    if w > 0 && h > 0 {
                        gpu.resize(w as u32, h as u32);
                        editor.win_w    = gpu.win_w;
                        editor.win_h    = gpu.win_h;
                        editor.ui.win_w = editor.win_w;
                        editor.ui.win_h = editor.win_h;
                        editor.layout_panels();
                        should_redraw = true;
                    }
                }

                Event::Window { win_event: SdlWindowEvent::FocusGained, .. } => {
                    editor.is_our_window_focused = true;
                }
                Event::Window { win_event: SdlWindowEvent::FocusLost, .. } => {
                    editor.is_our_window_focused = false;
                }

                Event::KeyDown { keycode, scancode, keymod, repeat: _, .. } => {
                    input_this_frame = true;

                    let Some(key) = sdl_to_key_input(keycode, scancode, keymod) else { continue };

                    last_input_time = Instant::now();

                    if key.mods.ctrl && key.mods.shift
                        && matches!(key.logical, LogicalKey::Named(NamedKey::F12))
                    {
                        println!("[Trying to Hot reload]");
                        app.force_try_reload();
                        should_redraw = true;
                        continue;
                    }

                    hide_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);
                    editor.reset_blink();

                    should_redraw |= run_key_hooks_and_command(
                        editor,
                        gpu,
                        &mut app.command_table,
                        &mut app.keymap,
                        key,
                    );
                }

                Event::TextInput { ref text, .. } => {
                    input_this_frame = true;

                    let Mods { ctrl, alt, .. } = app.mods;
                    if ctrl || alt { continue; }

                    let Some(c) = text.chars().next() else { continue };

                    last_input_time = Instant::now();
                    hide_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);
                    editor.reset_blink();

                    should_redraw |= run_key_hooks_and_command_impl(
                        editor, gpu, &mut app.command_table, &mut app.keymap,
                        None,   // key_input
                        Some(basic_character_atom),
                        Some(c)
                    );
                }

                Event::MouseWheel { y, .. } => {
                    input_this_frame = true;

                    last_input_time = Instant::now();

                    if app.mods.ctrl {
                        let dy = y as f32;
                        let new = (editor.scale + dy * 0.075).clamp(MIN_SCALE, MAX_SCALE);
                        editor::rescale(editor, new);
                        should_redraw = true;
                        continue;
                    }

                    show_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);

                    let dy = y as f32*20.5;

                    let mouse_wheel_scrolled = editor.hooks.mouse_wheel_scrolled;
                    let (
                        custom_window_redraw_requested,
                        should_short_circuit
                    ) = mouse_wheel_scrolled.map_or(
                        (false, false),
                        |f| f(&mut make_command_context!(), dy)
                    );

                    should_redraw |= custom_window_redraw_requested;
                    if should_short_circuit {
                        continue;
                    }

                    let (mx, my) = editor.mouse_pos;
                    let Some(panel_id) = editor.panel_at(mx, my) else {
                        continue;
                    };
                    let PanelKind::Leaf { view_id } = editor.panels[panel_id].kind else {
                        continue;
                    };

                    let rect    = editor.panels[panel_id].rect;
                    let buf_id  = editor.views[view_id].buffer_id;
                    let total   = editor.buffers[buf_id].text.len_lines();
                    let line_h  = editor.line_h();

                    let old_scroll = editor.views[view_id].scroll;
                    let new_scroll = (old_scroll - dy * 3.0).max(0.0); // @Tune
                    let max_scroll = editor.max_scroll_of(view_id);
                    editor.views[view_id].scroll = new_scroll.min(max_scroll);

                    // Drag cursor if it went off screen
                    let (cur_line, cur_col) = editor.buffers[buf_id]
                        .cursor_line_col(&editor.views[view_id].cursor);

                    let scroll     = editor.views[view_id].scroll;
                    let first_vis  = (scroll / line_h) as u32;
                    let last_vis = (((scroll + rect.h) / line_h) as usize)
                        .saturating_sub(1)
                        .min(total.saturating_sub(1) as usize) as u32;

                    let new_line = if cur_line < first_vis {
                        first_vis
                    } else if cur_line > last_vis {
                        last_vis.min(total.saturating_sub(1) as u32) as u32
                    } else {
                        cur_line  // Still visible, don't move
                    };

                    if new_line != cur_line {
                        let mut cursor = editor.views[view_id].active_cursor().clone();
                        editor.buffers[buf_id].set_cursor_line_col(new_line, cur_col, &mut cursor.cursor);

                        editor.views[view_id].cursor_target_line = new_line;
                        editor.views[view_id].cursor_target_col  = cur_col;
                        editor.views[view_id].normal_cursor      = cursor;

                        editor.snap_cursor_to_target(view_id, new_line, cur_col, rect);
                    }

                    should_redraw |= true; // @Cleanup
                }

                Event::MouseButtonDown { mouse_btn: MouseButton::Left, .. } => {
                    input_this_frame = true;

                    last_input_time = Instant::now();
                    show_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);

                    {
                        let mut cx = make_command_context!(reset None);
                        should_redraw |= editor_handle_left_mouse_click(&mut cx);
                    }

                    editor.mouse_left_pressed = true;

                    editor.ui.update_interaction(
                        [editor.mouse_pos.0, editor.mouse_pos.1],
                        editor.mouse_left_pressed,
                    );
                }

                Event::MouseButtonUp { mouse_btn: MouseButton::Left, .. } => {
                    input_this_frame = true;

                    show_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);
                    editor.mouse_left_pressed = false;

                    editor.ui.update_interaction(
                        [editor.mouse_pos.0, editor.mouse_pos.1],
                        editor.mouse_left_pressed,
                    );
                }

                Event::MouseMotion { x, y, .. } => {
                    input_this_frame = true;

                    last_input_time = Instant::now();
                    show_mouse_cursor(&sdl, &mut editor.is_mouse_cursor_visible);
                    editor.mouse_pos = (x as f32, y as f32);

                    editor.ui.update_interaction(
                        [editor.mouse_pos.0, editor.mouse_pos.1],
                        editor.mouse_left_pressed,
                    );

                    let (custom_redraw, short_circuit) = run_hook!(
                        editor.hooks.mouse_moved,
                        &mut make_command_context!(reset None)
                    );
                    should_redraw |= custom_redraw;
                    if short_circuit { continue; }

                    if editor.mouse_left_pressed {
                        let mut cx = make_command_context!(reset None);
                        should_redraw |= editor_handle_left_mouse_click(&mut cx);
                    }
                }

                _ => {}
            }

            if app.editor.window_event_finish() {
                should_redraw = true;
            }
        }

        //
        // Render
        //
        if should_redraw {
            let now = Instant::now();
            let dt = now.duration_since(app.editor.last_frame_time).as_secs_f32().min(0.05);
            app.editor.last_frame_time = now;

            renderer_requested_redraw = render_frame(
                &mut app.editor,
                app.gpu.as_mut().unwrap(),
                &mut app.command_table,
                &mut app.keymap,
                dt,
            );

            if !input_this_frame && app.editor.config.vsync {
                let elapsed = app.editor.last_frame_time.elapsed();
                if elapsed < target_frame_time {
                    let remaining_micros = (target_frame_time - elapsed).as_micros();

                    // Sleep for most of the time to yield the CPU, but wake up ~2ms early
                    if remaining_micros > 1500 {
                        let sleep_ms = ((remaining_micros - 1500) / 1000) as u32;
                        if let Some(early_event_) = event_pump.wait_event_timeout(sleep_ms) {
                            early_event = Some(early_event_);
                        }
                    }

                    // Spin-wait (busy loop) the final <2 milliseconds for perfect precision
                    while app.editor.last_frame_time.elapsed() < target_frame_time {
                        std::hint::spin_loop();
                    }
                }
            }
        } else {
            renderer_requested_redraw = false;
        }
    }

    //
    // Exiting
    //
    if let Some(hook) = app.editor.hooks.exiting {
        hook(&mut app.editor);
    }

    _ = save_session(&app.editor, &default_session_path());
}

fn run_key_hooks_and_command_impl(
    editor:        &mut Editor,
    gpu:           &mut Gpu,
    command_table: &mut CommandTable,
    keymap:        &mut Keymap,

    key_input:     Option<KeyInput>,
    command_atom:  Option<CommandAtom>,

    text_input:    Option<char>
) -> bool {
    let skip_basic_character_atom = text_input.is_none();

    let mut should_redraw = false;

    define_command_context_macro!(
        editor = editor,
        gpu = gpu,
        command_table = command_table,
        keymap = keymap
    );

    let (redraw, short_circuit) = run_hook!(
        editor.hooks.key_pressed,
        &mut make_command_context!(opt key_input, opt text text_input)
    );
    should_redraw |= redraw;
    if short_circuit { return should_redraw; }

    let Some(atom) = command_atom.or_else(|| key_input.as_ref().and_then(|k| keymap.lookup(k))) else {
        return should_redraw;
    };
    if skip_basic_character_atom && atom == keymap.basic_character_atom {
        return should_redraw;
    }

    let Some(&command) = command_table.get(&atom) else {
        eprintln!("[Undefined command]: {}", command_table.resolve(atom));
        return should_redraw;
    };

    let (redraw, short_circuit) = run_hook!(
        editor.hooks.pre_command_execution,
        &mut make_command_context!(opt key_input, opt text text_input),
        atom
    );
    should_redraw |= redraw;
    if short_circuit { return should_redraw; }

    (command.func)(&mut make_command_context!(opt key_input, opt text text_input));

    let (redraw, _) = run_hook!(
        editor.hooks.post_command_execution,
        &mut make_command_context!(opt key_input, opt text text_input),
        atom
    );
    should_redraw |= redraw;

    should_redraw |= true;  // Command ran, always redraw

    should_redraw
}

fn run_key_hooks_and_command(
    editor:        &mut Editor,
    gpu:           &mut Gpu,
    command_table: &mut CommandTable,
    keymap:        &mut Keymap,
    key_input:     KeyInput,
) -> bool {
    run_key_hooks_and_command_impl(
        editor, gpu, command_table, keymap, Some(key_input),
        None, None
    )
}

fn render_frame(editor: &mut Editor, gpu: &mut Gpu, command_table: &mut CommandTable, keymap: &mut Keymap, dt: f32) -> bool {
    define_command_context_macro!(
        editor = editor,
        gpu = gpu,
        command_table = command_table,
        keymap = keymap
    );

    tracy::frame_mark();
    let _render_span = tracy::span!("Render Frame");

    ShouldRequestFrameRedraw::begin_frame(&mut editor.redraw_reasons);

    let mut redraw = ShouldRequestFrameRedraw::No;

    editor.ui.begin_frame(gpu.win_w, gpu.win_h);

    editor.frame_count += 1;
    editor.fps_acc += editor.frame_time_in_seconds;

    if editor.fps_acc >= 0.5 {
        editor.fps       = editor.frame_count as f32 / editor.fps_acc;
        editor.build_us  = editor.build_us_acc  / editor.frame_count as f32;
        editor.render_us = editor.render_us_acc / editor.frame_count as f32;
        editor.relex_us  = editor.relex_us_acc  / editor.frame_count as f32;

        editor.frame_count   = 0;
        editor.fps_acc  = 0.0;
        editor.build_us_acc  = 0.0;
        editor.relex_us_acc  = 0.0;
        editor.render_us_acc = 0.0;
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

    let mut leaf_panels = Default::default();
    collect_leaves(editor, editor.root_panel, &mut leaf_panels);

    for (_panel_id, view_id, ..) in &leaf_panels {
        let view = &editor.views[*view_id];
        let char_index = view.cursor.char_index;
        editor.last_cursor_char_indexes.insert(*view_id, char_index as _);
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
        if is_dirty {
            rebuild_text_layout(editor, gpu, view_id, rect);
        }

        redraw = redraw.or_if(is_dirty, "Layout rebuild", &mut editor.redraw_reasons);
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
        let show_cursor = editor.views[view_id].is_cursor_visible();

        gpu::push_clip(gpu, r.x, r.y, r.w, r.h);
        let t1 = Instant::now();
        {
            render_text_layout(editor, gpu, view_id, show_cursor);
            render_panel_bar(gpu, editor, view_id);
            render_completion_dropdown(gpu, editor, view_id);
        }
        editor.render_us_acc += t1.elapsed().as_micros() as f32;

        gpu::pop_clip(gpu);
    }

    if let Some(drew_all_leaf_panels_hook) = editor.hooks.drew_all_leaf_panels {
        let _tracy = tracy::span!("drew_all_leaf_panels_hook");

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
        gpu::draw_rect_rounded(gpu, x, y, w, h, editor.cursor_radius(), palette().cursor.with_alpha(fc.alpha * 0.6));

        redraw = redraw.or_msg("Flying cursor", &mut editor.redraw_reasons);
    }

    let t1 = Instant::now();
    render_messager(gpu, editor);
    editor.render_us_acc += t1.elapsed().as_micros() as f32;

    {
        editor.ui.end_frame();
        editor.ui.layout(|text, font_size| {
            let w = text.chars()
                .filter_map(|c| gpu::get_glyph_no_upload(gpu, c, font_size))
                .map(|g| g.advance)
                .sum();

            [w, font_size]
        });

        gpu::push_overlay_mode(gpu);
        ui::render(&editor.ui, gpu);
        gpu::pop_overlay_mode(gpu);
    }

    for buffer in editor.buffers.values_mut() {
        //
        // No buffer can be dirty now!
        //
        buffer.is_dirty = false;
    }

    wait_for_atlas_upload(gpu);
    _ = gpu.submit_frame();

    redraw = redraw.or_if(editor.messager.count != editor.last_messager_count, "Messager animation", &mut editor.redraw_reasons);
    redraw = redraw.or_if(editor.messager.count != 0, "Messager animation", &mut editor.redraw_reasons); // nocheckin

    if let Some(inside_redraw_should_request_redraw) = editor.hooks.at_the_end_of_redraw_should_request_redraw {
        redraw |= inside_redraw_should_request_redraw(editor);
    }

    if redraw.into() {
        // println!("Renderer requesting redraw: {}", redraw.display(&editor.redraw_reasons));
        return true;
    }

    false
}

#[inline]
fn show_mouse_cursor(sdl: &sdl2::Sdl, is_mouse_cursor_currently_visible: &mut bool) {
    if *is_mouse_cursor_currently_visible { return }

    sdl.mouse().show_cursor(true);
    *is_mouse_cursor_currently_visible = true;
}

#[inline]
fn hide_mouse_cursor(sdl: &sdl2::Sdl, is_mouse_cursor_currently_visible: &mut bool) {
    if !*is_mouse_cursor_currently_visible { return }

    sdl.mouse().show_cursor(false);
    *is_mouse_cursor_currently_visible = false;
}

//
//
//
//
//
//
// SDL INPUT
//
//
//
//
//
//

#[inline]
fn mods_from_sdl(keymod: sdl2::keyboard::Mod) -> Mods {
    use sdl2::keyboard::Mod;
    Mods {
        ctrl:  keymod.intersects(Mod::LCTRLMOD  | Mod::RCTRLMOD),
        shift: keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD),
        alt:   keymod.intersects(Mod::LALTMOD   | Mod::RALTMOD),
    }
}

pub fn sdl_to_key_input(
    keycode:  Option<sdl2::keyboard::Keycode>,
    scancode: Option<sdl2::keyboard::Scancode>,
    keymod:   sdl2::keyboard::Mod,
) -> Option<KeyInput> {
    use sdl2::keyboard::{Keycode as Kc, Mod, Scancode as Sc};

    let ctrl  = keymod.intersects(Mod::LCTRLMOD  | Mod::RCTRLMOD);
    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
    let alt   = keymod.intersects(Mod::LALTMOD   | Mod::RALTMOD);
    let mods  = Mods { ctrl, shift, alt };

    let logical = match keycode? {
        Kc::Return | Kc::KpEnter => LogicalKey::Named(NamedKey::Enter),
        Kc::Backspace            => LogicalKey::Named(NamedKey::Backspace),
        Kc::Delete               => LogicalKey::Named(NamedKey::Delete),
        Kc::Escape               => LogicalKey::Named(NamedKey::Escape),
        Kc::Tab                  => LogicalKey::Named(NamedKey::Tab),
        Kc::Insert               => LogicalKey::Named(NamedKey::Insert),
        Kc::Space                => LogicalKey::Named(NamedKey::Space),
        Kc::Left                 => LogicalKey::Named(NamedKey::Left),
        Kc::Right                => LogicalKey::Named(NamedKey::Right),
        Kc::Up                   => LogicalKey::Named(NamedKey::Up),
        Kc::Down                 => LogicalKey::Named(NamedKey::Down),
        Kc::Home                 => LogicalKey::Named(NamedKey::Home),
        Kc::End                  => LogicalKey::Named(NamedKey::End),
        Kc::PageUp               => LogicalKey::Named(NamedKey::PageUp),
        Kc::PageDown             => LogicalKey::Named(NamedKey::PageDown),
        Kc::F1  => LogicalKey::Named(NamedKey::F1),
        Kc::F2  => LogicalKey::Named(NamedKey::F2),
        Kc::F3  => LogicalKey::Named(NamedKey::F3),
        Kc::F4  => LogicalKey::Named(NamedKey::F4),
        Kc::F5  => LogicalKey::Named(NamedKey::F5),
        Kc::F6  => LogicalKey::Named(NamedKey::F6),
        Kc::F7  => LogicalKey::Named(NamedKey::F7),
        Kc::F8  => LogicalKey::Named(NamedKey::F8),
        Kc::F9  => LogicalKey::Named(NamedKey::F9),
        Kc::F10 => LogicalKey::Named(NamedKey::F10),
        Kc::F11 => LogicalKey::Named(NamedKey::F11),
        Kc::F12 => LogicalKey::Named(NamedKey::F12),

        // Printable: SDL2 keycode IS the Unicode codepoint of the base char
        kc => {
            let base = char::from_u32(kc.into_i32() as u32)?;
            if !base.is_ascii_graphic() { return None; }
            // Apply shift for printable chars - TextInput handles actual
            // text insertion; this is only for keymap lookup
            let c = if shift { shift_char(base) } else { base };
            LogicalKey::Char(c)
        }
    };

    let physical = scancode.and_then(|sc| match sc {
        Sc::A => Some(PhysicalKey::A), Sc::B => Some(PhysicalKey::B),
        Sc::C => Some(PhysicalKey::C), Sc::D => Some(PhysicalKey::D),
        Sc::E => Some(PhysicalKey::E), Sc::F => Some(PhysicalKey::F),
        Sc::G => Some(PhysicalKey::G), Sc::H => Some(PhysicalKey::H),
        Sc::I => Some(PhysicalKey::I), Sc::J => Some(PhysicalKey::J),
        Sc::K => Some(PhysicalKey::K), Sc::L => Some(PhysicalKey::L),
        Sc::M => Some(PhysicalKey::M), Sc::N => Some(PhysicalKey::N),
        Sc::O => Some(PhysicalKey::O), Sc::P => Some(PhysicalKey::P),
        Sc::Q => Some(PhysicalKey::Q), Sc::R => Some(PhysicalKey::R),
        Sc::S => Some(PhysicalKey::S), Sc::T => Some(PhysicalKey::T),
        Sc::U => Some(PhysicalKey::U), Sc::V => Some(PhysicalKey::V),
        Sc::W => Some(PhysicalKey::W), Sc::X => Some(PhysicalKey::X),
        Sc::Y => Some(PhysicalKey::Y), Sc::Z => Some(PhysicalKey::Z),
        Sc::Num0 => Some(PhysicalKey::N0), Sc::Num1 => Some(PhysicalKey::N1),
        Sc::Num2 => Some(PhysicalKey::N2), Sc::Num3 => Some(PhysicalKey::N3),
        Sc::Num4 => Some(PhysicalKey::N4), Sc::Num5 => Some(PhysicalKey::N5),
        Sc::Num6 => Some(PhysicalKey::N6), Sc::Num7 => Some(PhysicalKey::N7),
        Sc::Num8 => Some(PhysicalKey::N8), Sc::Num9 => Some(PhysicalKey::N9),
        Sc::Minus     => Some(PhysicalKey::Minus),
        Sc::Equals    => Some(PhysicalKey::Equals),
        Sc::LeftBracket  => Some(PhysicalKey::LeftBracket),
        Sc::RightBracket => Some(PhysicalKey::RightBracket),
        Sc::Backslash => Some(PhysicalKey::Backslash),
        Sc::Semicolon => Some(PhysicalKey::Semicolon),
        Sc::Apostrophe => Some(PhysicalKey::Apostrophe),
        Sc::Grave  => Some(PhysicalKey::Grave),
        Sc::Comma  => Some(PhysicalKey::Comma),
        Sc::Period => Some(PhysicalKey::Period),
        Sc::Slash  => Some(PhysicalKey::Slash),
        Sc::Kp0 => Some(PhysicalKey::Kp0), Sc::Kp1 => Some(PhysicalKey::Kp1),
        Sc::Kp2 => Some(PhysicalKey::Kp2), Sc::Kp3 => Some(PhysicalKey::Kp3),
        Sc::Kp4 => Some(PhysicalKey::Kp4), Sc::Kp5 => Some(PhysicalKey::Kp5),
        Sc::Kp6 => Some(PhysicalKey::Kp6), Sc::Kp7 => Some(PhysicalKey::Kp7),
        Sc::Kp8 => Some(PhysicalKey::Kp8), Sc::Kp9 => Some(PhysicalKey::Kp9),
        Sc::KpPlus     => Some(PhysicalKey::KpPlus),
        Sc::KpMinus    => Some(PhysicalKey::KpMinus),
        Sc::KpMultiply => Some(PhysicalKey::KpMultiply),
        Sc::KpDivide   => Some(PhysicalKey::KpDivide),
        Sc::KpEnter    => None,  // Already handled as NamedKey::Enter above
        Sc::KpPeriod   => Some(PhysicalKey::KpPeriod),
        _ => None,
    });

    Some(KeyInput { logical, physical, mods })
}

const fn shift_char(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!', '2' => '@', '3' => '#', '4' => '$', '5' => '%',
        '6' => '^', '7' => '&', '8' => '*', '9' => '(', '0' => ')',
        '-' => '_', '=' => '+', '[' => '{', ']' => '}', '\\' => '|',
        ';' => ':', '\'' => '"', ',' => '<', '.' => '>', '/' => '?',
        '`' => '~',
        c   => c,
    }
}
