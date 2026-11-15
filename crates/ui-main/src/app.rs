//! UI 胶水层：slint 回调接线、工作线程（std::thread + `invoke_from_event_loop`，
//! 无 tokio / 无轮询守护）、对话层状态机、应用流程（旧 C#
//! `IndexViewModel.ApplyCurrentModsAsync` 的语义对齐：备份先行、热应用优先、
//! `game-not-running` 自动离线回退）。
//!
//! 线程纪律：`Shared` 只在事件循环线程（回调 / marshal 闭包）里加锁访问，
//! 工作线程只带快照数据；锁内不做 slint setter 调用（先取数、drop 守卫、再刷 UI）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use hotapply::{ensure_hooked, game_running};
use save::{
    AppliedMod, ApplyOptions, Estate, EstateFile, ModInfo, ModSource, Presets, SaveError,
    SaveRoots, apply_offline, backup_estate_dir, clean_old_backups, discover_estates, scan_mod_dir,
};

use crate::model::{
    self, AppConfig, AppliedRow, applied_from_mod, display_title, dlc_names,
    file_only_game_running_text, file_only_ok_text, filter_indices, game_mode_cn,
    hotapply_ritual_text, is_dirty, mark_present, offline_fallback_text, source_cn,
};

slint::include_modules!();

const CFG_FILE_NAME: &str = "dd-mrm.json";
const BACKUP_DIR_NAME: &str = "SaveBak";
const LOG_DIR_NAME: &str = "logs";
const BACKUP_MAX_AGE: Duration = Duration::from_secs(15 * 24 * 3600);

type State = Arc<Mutex<Shared>>;

/// 对话层状态机（与 .slint 的 dlg-kind 一一对应）。
#[derive(Clone, Debug, PartialEq)]
enum Dialog {
    None,
    /// 脏状态下切换存档：0 = 放弃修改并切换 / 1 = 取消。
    ConfirmSwitch {
        target: usize,
    },
    /// 脏状态下退出：0 = 退出 / 1 = 取消。
    ConfirmExit,
    /// 备份失败（旧工具 MessageBox「坚持应用/取消」的三态扩展）：
    /// 0 = 重试备份 / 1 = 跳过备份继续 / 2 = 取消。
    BackupFail {
        estate: usize,
    },
    /// 存预设（输入框，Enter = 确认）；重命名复用本对话（title=重命名预设 + rename_from）。
    PresetSave,
    /// 应用预设（列表）：0 = 载入 / 1 = 取消；双击/回车 = 载入。
    PresetLoad,
    /// 预设管理（列表）：0 = 重命名 / 1 = 删除 / 2 = 关闭；双击 = 重命名。
    PresetManage,
    /// 设置。
    Settings,
    /// 错误披露（带待展示队列）。
    Error,
    /// 缺失清单（dlg 7）：0 = 全部移除（固定全量，与过滤无关）/ 末位 = 关闭。
    Missing,
    /// 保存预设内双击行覆盖：0 = 覆盖 / 末位 = 取消（复用 dlg 1 消息框版式）。
    ConfirmPresetOverwrite {
        name: String,
    },
}

impl Dialog {
    fn kind(&self) -> i32 {
        match self {
            Dialog::None => 0,
            Dialog::ConfirmSwitch { .. }
            | Dialog::ConfirmExit
            | Dialog::ConfirmPresetOverwrite { .. } => 1,
            Dialog::BackupFail { .. } => 2,
            Dialog::PresetSave => 3,
            Dialog::PresetLoad => 4,
            Dialog::PresetManage => 8,
            Dialog::Settings => 5,
            Dialog::Error => 6,
            Dialog::Missing => 7,
        }
    }
}

/// 一个存档在 UI 里的元数据（重扫时加载一次；写入永远用 `estate.persist_path`，
/// 该路径只可能来自 `discover_estates` —— GC3 白名单收口，ruling 2）。
struct EstateData {
    estate: Estate,
    label: String,
    mode: String,
    /// DLC 名称列表（详情区 chips；权属列表条目转中文短名，空 = 无 DLC）。
    dlcs: Vec<String>,
    count: usize,
    ok: bool,
    error: String,
}

struct Shared {
    cfg: AppConfig,
    cfg_path: PathBuf,
    savebak: PathBuf,
    estates: Vec<EstateData>,
    installed: Vec<ModInfo>,
    /// 过滤后的已安装视图（installed 下标）。
    view: Vec<usize>,
    applied: Vec<AppliedRow>,
    /// 上次从文件载入 / 上次应用成功的基线（脏判定参照）。
    baseline: Vec<AppliedMod>,
    current: Option<usize>,
    presets: Presets,
    /// 重命名中的预设旧名（PresetSave 对话框复用为重命名输入框时非 None）。
    preset_rename_from: Option<String>,
    applying: bool,
    dialog: Dialog,
    /// 启动期错误队列（配置/预设损坏等），逐个弹窗披露。
    pending_errors: Vec<String>,
}

fn st_lock(st: &State) -> MutexGuard<'_, Shared> {
    // 锁中毒（某线程持锁崩溃）不放弃数据 —— 旧工具「零崩溃」纪律。
    st.lock().unwrap_or_else(|p| p.into_inner())
}

/// 依据当前 installed 扫描结果重标 applied 行的 present（避开同结构体双借用）。
fn remark(s: &mut Shared) {
    let installed = std::mem::take(&mut s.installed);
    mark_present(&mut s.applied, &installed);
    s.installed = installed;
}

fn save_error_cn(e: &SaveError) -> String {
    match e {
        SaveError::Io { path, source } => format!("读写失败：{}：{source}", path.display()),
        SaveError::Dson(d) => format!("存档解析失败：{d}"),
        SaveError::Whitelist { path, reason } => {
            format!("路径校验失败：{}（{reason}）", path.display())
        }
        SaveError::Shape(s) => format!("存档结构异常：{s}"),
        SaveError::Config { path, reason } => format!("配置错误：{}（{reason}）", path.display()),
        SaveError::Backup { path, reason } => format!("备份失败：{}（{reason}）", path.display()),
    }
}

/// 工作线程回传的应用结果（UI 线程消费）。
enum ApplyMsg {
    /// 终态文案。`committed` = 存档态确实已落定（热应用成功/离线写成功）；
    /// 仅 committed=true 时基线 := 本次应用的列表，失败文案不清脏标记。
    Status { text: String, committed: bool },
    /// 备份失败 → 三选一弹窗。
    BackupFail(String),
    /// 其它错误 → 错误弹窗。
    Error(String),
}

// ============================================================ 入口

pub fn run() {
    let app = AppWindow::new().expect("窗口创建失败");
    clamp_init_size(&app);
    let exe = model::exe_dir();
    let cfg_path = exe.join(CFG_FILE_NAME);
    let savebak = exe.join(BACKUP_DIR_NAME);

    // 目录就绪（备份 + 热应用日志，repo logs/ 同款约定）。
    let _ = std::fs::create_dir_all(&savebak);
    let _ = std::fs::create_dir_all(exe.join(LOG_DIR_NAME));

    let mut pending_errors: Vec<String> = Vec::new();

    // 配置：缺失 → 首跑默认；损坏 → 弹窗披露（绝不静默重置）。
    let cfg = match AppConfig::load(&cfg_path) {
        Ok(Some(c)) => c,
        Ok(None) => AppConfig::default(),
        Err(msg) => {
            pending_errors.push(format!("配置读取失败：{msg}"));
            AppConfig::default()
        }
    };

    let presets_path = cfg.presets_path.clone();
    let presets = match Presets::load(&presets_path) {
        Ok(Some(p)) => p,
        Ok(None) => Presets::default(),
        Err(e) => {
            pending_errors.push(format!(
                "预设文件读取失败（{}）：{e}",
                presets_path.display()
            ));
            Presets::default()
        }
    };

    let st: State = Arc::new(Mutex::new(Shared {
        cfg,
        cfg_path,
        savebak: savebak.clone(),
        estates: Vec::new(),
        installed: Vec::new(),
        view: Vec::new(),
        applied: Vec::new(),
        baseline: Vec::new(),
        current: None,
        presets,
        preset_rename_from: None,
        applying: false,
        dialog: Dialog::None,
        pending_errors,
    }));

    wire_callbacks(&app, &st);

    refresh_estates(&app, &st);
    refresh_installed(&app, &st);
    refresh_applied(&app, &st);
    refresh_presets(&app, &st);

    // 启动期错误逐个披露。
    maybe_show_next_error(&app, &st);

    // 旧备份清理（15 天，旧 BackupManager 口径）；失败可忽略（清理性质）。
    {
        let dir = savebak.clone();
        std::thread::spawn(move || {
            let _ = clean_old_backups(&dir, BACKUP_MAX_AGE);
        });
    }

    rescan(&app, &st);
    refresh_game_state(app.as_weak(), st_lock(&st).cfg.hotapply);

    // 自动化验证钩子（用户 2026-11-05 授权自助截图验证）：设 DDMRM_DEBUG_VIEW=
    // settings 启动即开设置对话框；settings-off 同款但开关预设为关。不设即无影响。
    match std::env::var("DDMRM_DEBUG_VIEW").as_deref() {
        Ok("settings") | Ok("settings-off") => {
            let off = std::env::var("DDMRM_DEBUG_VIEW").as_deref() == Ok("settings-off");
            app.invoke_settings_clicked();
            if off {
                app.set_cfg_hotapply(false); // 快照后覆写 → 显示关闭态
            }
        }
        _ => {}
    }

    let _ = app.run();
}

// 方案 A（用户 2026-11-05 选定）：默认 1280×860，超出主屏逻辑工作区 90% 时收窄。
// 读主显视器有效 DPI 把物理工作区换算成逻辑像素；窗口 show 前生效，无闪烁。
// 多显示器混合 DPI 且开在副屏时近似（按主屏算），ponytail 范围外。
#[cfg(windows)]
fn clamp_init_size(app: &AppWindow) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if hmon.is_null() || unsafe { GetMonitorInfoW(hmon, &mut mi) } == 0 {
        return;
    }
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } != 0 {
        return;
    }
    let scale = (dpi_x as f32 / 96.0).max(1.0);
    let avail_w = (mi.rcWork.right - mi.rcWork.left) as f32 / scale;
    let avail_h = (mi.rcWork.bottom - mi.rcWork.top) as f32 / scale;
    app.set_init_width(1280.0_f32.min(avail_w * 0.9));
    app.set_init_height(860.0_f32.min(avail_h * 0.9));
}

// ============================================================ 回调接线

fn wire_callbacks(app: &AppWindow, st: &State) {
    // ---- 列表交互 ----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_estate_selected(move |idx| {
            let app = weak.unwrap();
            let idx = idx.max(0) as usize;
            let blocked;
            {
                let s = st_lock(&st);
                blocked = s.applying || !matches!(s.dialog, Dialog::None) || idx >= s.estates.len();
            }
            if blocked {
                sync_selection(&app, &st);
                return;
            }
            {
                let s = st_lock(&st);
                if s.current == Some(idx) {
                    return;
                }
                if is_dirty(&s.baseline, &s.applied) {
                    drop(s);
                    open_dialog(
                        &app,
                        &st,
                        Dialog::ConfirmSwitch { target: idx },
                        "未保存的修改",
                        "当前存档有未应用的修改，切换将丢弃这些修改。",
                        vec!["放弃修改并切换", "取消"],
                    );
                    sync_selection(&app, &st); // 弹窗期间视觉选回已载入档
                    return;
                }
            }
            load_estate(&app, &st, idx);
        });
    }

    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_installed_activated(move |view_idx| {
            let app = weak.unwrap();
            let idx = {
                let s = st_lock(&st);
                s.view.get(view_idx.max(0) as usize).copied()
            };
            let Some(i) = idx else { return };
            add_installed(&app, &st, i, None);
        });
    }

    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_applied_move(move |idx, delta| {
            let app = weak.unwrap();
            if delta == 0 {
                return;
            }
            let moved;
            {
                let mut s = st_lock(&st);
                if s.applying || !matches!(s.dialog, Dialog::None) {
                    return;
                }
                let i = idx.max(0) as usize;
                let j = i as isize + delta.signum() as isize;
                if i < s.applied.len() && j >= 0 && (j as usize) < s.applied.len() {
                    s.applied.swap(i, j as usize);
                    moved = Some(j as i32);
                } else {
                    moved = None;
                }
            }
            if let Some(j) = moved {
                refresh_applied(&app, &st);
                app.set_current_applied(j);
            }
        });
    }

    // 回车/Delete/双击移除一个已应用项。
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_applied_activated(move |idx| remove_applied(&weak.unwrap(), &st, idx));
    }

    // ---- 过滤 ----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_filter_changed(move |text| {
            let app = weak.unwrap();
            {
                let mut s = st_lock(&st);
                s.view = filter_indices(&s.installed, &text, &s.applied);
            }
            refresh_installed(&app, &st);
            app.set_current_installed(-1);
        });
    }

    // ---- 装列表视口滚动 → 可见行图标插队解码（数百 mod 规模） ----
    {
        let weak = app.as_weak();
        app.on_icons_visible(move |offset_px, view_h_px| {
            let app = weak.unwrap();
            request_visible_icons(&app, offset_px, view_h_px);
        });
    }

    // ---- 拖拽（旧 GongSolutions 同款：拖动显示插入线，松手落位）----
    {
        let weak = app.as_weak();
        app.on_drag_update(move |win_y, lv_y, content_y| {
            let app = weak.unwrap();
            let len = applied_model_len(&app);
            let idx = drop_index(win_y, lv_y, content_y, len).map_or(-1, |v| v as i32);
            if app.get_drop_index() != idx {
                app.set_drop_index(idx);
            }
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_drag_finish(move |src, idx, win_y, lv_y, content_y| {
            let app = weak.unwrap();
            app.set_drop_index(-1);
            if busy(&st) {
                return;
            }
            let len = applied_model_len(&app);
            let Some(target) = drop_index(win_y, lv_y, content_y, len) else {
                return;
            };
            let i = idx.max(0) as usize;
            match src {
                // 装列表 → 应用列表：行回调传的是视图下标（装列表模型由过滤视图构建），
                // 必须经 s.view 映射到 installed 下标——与 on_installed_activated 同口径。
                1 => {
                    let vi = st_lock(&st).view.get(i).copied();
                    let Some(vi) = vi else { return };
                    add_installed(&app, &st, vi, Some(target));
                }
                // 应用列表内：按落点重排（旧 DropHandler：源在落点前则回退一格）。
                2 => reorder_applied(&app, &st, i, target),
                _ => {}
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_open_mod_dir(move |path| {
            let app = weak.unwrap();
            let p = PathBuf::from(path.as_str());
            if p.is_dir() {
                let _ = std::process::Command::new("explorer.exe").arg(&p).spawn();
            } else {
                set_status(&app, "mod 目录不存在。", 3);
            }
        });
    }

    // ---- 工具栏 ----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_rescan_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            rescan(&app, &st);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_clear_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            {
                let ft = app.get_installed_filter().to_string();
                let mut s = st_lock(&st);
                if s.current.is_none() {
                    drop(s);
                    set_status(&app, "请先选择一个存档。", 3);
                    return;
                }
                s.applied.clear();
                s.view = filter_indices(&s.installed, &ft, &s.applied);
            }
            refresh_applied(&app, &st);
            refresh_installed(&app, &st);
            app.set_current_installed(-1);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_reset_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            {
                let ft = app.get_installed_filter().to_string();
                let mut s = st_lock(&st);
                if s.current.is_none() {
                    drop(s);
                    set_status(&app, "请先选择一个存档。", 3);
                    return;
                }
                s.applied = s.baseline.iter().map(AppliedRow::from_applied).collect();
                remark(&mut s);
                s.view = filter_indices(&s.installed, &ft, &s.applied);
            }
            refresh_applied(&app, &st);
            refresh_installed(&app, &st);
            app.set_current_installed(-1);
            set_status(&app, "已重置为存档文件中的列表。", 0);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_settings_clicked(move || {
            let app = weak.unwrap();
            if st_lock(&st).applying {
                return;
            }
            close_dialog(&app, &st); // 互斥：一次只开一个
            let (sr, lm, ws, ha) = {
                let s = st_lock(&st);
                (
                    s.cfg.roots.save_root.clone(),
                    s.cfg.roots.local_mods_dir.clone(),
                    s.cfg.roots.workshop_dir.clone(),
                    s.cfg.hotapply,
                )
            };
            app.set_cfg_save_root(path_text(sr).into());
            app.set_cfg_local_mods(path_text(lm).into());
            app.set_cfg_workshop(path_text(ws).into());
            app.set_cfg_hotapply(ha);
            open_dialog(
                &app,
                &st,
                Dialog::Settings,
                "设置",
                "",
                vec!["保存并重扫", "取消"],
            );
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_save_preset_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            if st_lock(&st).current.is_none() {
                set_status(&app, "请先选择一个存档。", 3);
                return;
            }
            refresh_presets(&app, &st);
            open_dialog(
                &app,
                &st,
                Dialog::PresetSave,
                "保存预设",
                "输入新名字 = 新增；双击已有行 = 覆盖（会先弹确认）：",
                vec!["新增", "取消"],
            );
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_apply_preset_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            if st_lock(&st).presets.presets.is_empty() {
                set_status(
                    &app,
                    "还没有任何预设；先用「已应用模组」标题栏的「保存预设」存一个。",
                    3,
                );
                return;
            }
            refresh_presets(&app, &st);
            open_dialog(
                &app,
                &st,
                Dialog::PresetLoad,
                "应用预设",
                "选中预设后点「载入」应用到当前已应用列表（会覆盖）：",
                vec!["载入", "关闭"],
            );
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_preset_manage_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            if st_lock(&st).presets.presets.is_empty() {
                set_status(
                    &app,
                    "还没有任何预设；先用「已应用模组」标题栏的「保存预设」存一个。",
                    3,
                );
                return;
            }
            refresh_presets(&app, &st);
            open_dialog(
                &app,
                &st,
                Dialog::PresetManage,
                "预设管理",
                "选中预设后点「重命名 / 删除」管理预设：",
                vec!["重命名", "删除", "关闭"],
            );
        });
    }
    {
        // 预设对话框行内动作（行下标 = 预设序）。
        let weak = app.as_weak();
        let st = st.clone();
        app.on_preset_load_row(move |idx| {
            on_preset_load_row(&weak.unwrap(), &st, idx);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_preset_rename_row(move |idx| {
            on_preset_rename_row(&weak.unwrap(), &st, idx);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_preset_delete_row(move |idx| {
            on_preset_delete_row(&weak.unwrap(), &st, idx);
        });
    }
    {
        // 保存预设对话框：行内「覆盖」/双击行（行下标 = 预设序）。
        let weak = app.as_weak();
        let st = st.clone();
        app.on_preset_overwrite_row(move |idx| {
            on_preset_overwrite_row(&weak.unwrap(), &st, idx);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_apply_clicked(move || {
            let app = weak.unwrap();
            if busy(&st) {
                return;
            }
            let cur = st_lock(&st).current;
            match cur {
                Some(idx) => start_apply(&app, &st, idx, false),
                None => set_status(&app, "请先在左侧选择一个存档。", 3),
            }
        });
    }

    // ---- 对话层 ----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_dlg_button_clicked(move |idx| on_dlg_button(&weak.unwrap(), &st, idx));
    }

    // ---- 设置：浏览 / 打开目录 ----
    {
        let weak = app.as_weak();
        app.on_browse_dir(move |which| {
            let app = weak.unwrap();
            if let Some(p) = rfd::FileDialog::new().set_title("选择目录").pick_folder() {
                let t = p.display().to_string();
                match which {
                    0 => app.set_cfg_save_root(t.into()),
                    1 => app.set_cfg_local_mods(t.into()),
                    _ => app.set_cfg_workshop(t.into()),
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_open_dir(move |which| {
            let app = weak.unwrap();
            let path = {
                let s = st_lock(&st);
                match which {
                    0 => s.cfg.roots.save_root.clone(),
                    1 => s.cfg.roots.local_mods_dir.clone(),
                    _ => s.cfg.roots.workshop_dir.clone(),
                }
            };
            match path {
                Some(p) if p.exists() => {
                    let _ = std::process::Command::new("explorer.exe").arg(&p).spawn();
                }
                _ => set_status(&app, "目录未配置或不存在。", 3),
            }
        });
    }

    // ---- 缺失清单（dlg 7；入口 = ⚠行 / 头部胶囊 / 状态条）----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_missing_open(move || {
            let app = weak.unwrap();
            // 只守 applying：本入口本身是开对话框（busy 的 dialog 守卫会自斥）。
            if st_lock(&st).applying {
                return;
            }
            let has_missing = st_lock(&st).applied.iter().any(|r| !r.present);
            if !has_missing {
                return;
            }
            app.set_missing_filter("".into());
            refresh_missing_model(&app, &st);
            open_dialog(
                &app,
                &st,
                Dialog::Missing,
                "缺失的模组",
                "以下模组在本地/工坊仓库中已不存在。应用时会原样写入存档；可逐项移除，或一次全部移除。",
                vec!["全部移除", "关闭"],
            );
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_missing_filter_changed(move |_| {
            refresh_missing_model(&weak.unwrap(), &st);
        });
    }
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.on_missing_remove(move |idx| {
            let app = weak.unwrap();
            // 过滤视图下标 → 模型行自带 name/source-cn（与 applied 匹配口径一致）。
            let row = app.get_missing_model().row_data(idx.max(0) as usize);
            let Some(m) = row else { return };
            let src = if m.source_cn.as_str() == source_cn(ModSource::Steam) {
                ModSource::Steam
            } else {
                ModSource::Local
            };
            {
                let mut s = st_lock(&st);
                // 只守 applying：该回调发生在缺失清单对话框内，dialog 守卫会自斥。
                if s.applying {
                    return;
                }
                if let Some(pos) = s
                    .applied
                    .iter()
                    .position(|r| r.name == m.name.as_str() && r.source == src)
                {
                    s.applied.remove(pos);
                    let ft = app.get_installed_filter().to_string();
                    s.view = filter_indices(&s.installed, &ft, &s.applied);
                }
            }
            refresh_missing_model(&app, &st);
            refresh_applied(&app, &st);
            refresh_installed(&app, &st);
            app.set_current_applied(-1);
        });
    }

    // ---- 关窗守卫（脏状态 / 应用中）----
    {
        let weak = app.as_weak();
        let st = st.clone();
        app.window().on_close_requested(move || {
            let app = weak.unwrap();
            let (applying, dirty, dlg_open) = {
                let s = st_lock(&st);
                (
                    s.applying,
                    is_dirty(&s.baseline, &s.applied),
                    !matches!(s.dialog, Dialog::None),
                )
            };
            if applying {
                set_status(&app, "正在应用，请稍候…", 2);
                return CloseRequestResponse::KeepWindowShown;
            }
            if dlg_open {
                return CloseRequestResponse::KeepWindowShown;
            }
            if dirty {
                open_dialog(
                    &app,
                    &st,
                    Dialog::ConfirmExit,
                    "未保存的修改",
                    "当前存档有未应用的修改，确定退出吗？",
                    vec!["退出", "取消"],
                );
                return CloseRequestResponse::KeepWindowShown;
            }
            CloseRequestResponse::HideWindow
        });
    }
}

fn busy(st: &State) -> bool {
    let s = st_lock(st);
    s.applying || !matches!(s.dialog, Dialog::None)
}

fn remove_applied(app: &AppWindow, st: &State, idx: i32) {
    let ft = app.get_installed_filter().to_string();
    {
        let mut s = st_lock(st);
        if s.applying || !matches!(s.dialog, Dialog::None) {
            return;
        }
        let i = idx.max(0) as usize;
        if i < s.applied.len() {
            s.applied.remove(i);
            // 移除后装列表视图放回该项（旧工具 MOVE 观感）。
            s.view = filter_indices(&s.installed, &ft, &s.applied);
        }
    }
    refresh_applied(app, st);
    refresh_installed(app, st);
    app.set_current_applied(-1);
    app.set_current_installed(-1);
}

fn path_text(p: Option<PathBuf>) -> String {
    p.map(|x| x.display().to_string()).unwrap_or_default()
}

fn non_empty_path(s: &str) -> Option<PathBuf> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(PathBuf::from(t))
    }
}

// ============================================================ 拖拽（旧 GongSolutions 语义）

/// 应用列表行高（appwindow.slint AppliedRow height:46px）。
const APPLIED_ROW_PX: f32 = 46.0;

/// 指针窗口 y（+ 列表窗口 y − content-y 滚动补偿）→ 插入位置 0..=len。
/// content-y 为 ListView 原始值（向下滚动为负，= 已滚过内容的相反数），
/// 内容坐标 = 视口相对坐标 − content-y；越界收拢到 0 / len。
fn drop_index(win_y: f32, lv_y: f32, content_y: f32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let rel = (win_y - lv_y - content_y) / APPLIED_ROW_PX;
    Some((rel.round() as isize).clamp(0, len as isize) as usize)
}

fn applied_model_len(app: &AppWindow) -> usize {
    app.get_applied_model().row_count()
}

/// 装列表第 `installed_idx` 项加入应用列表；`at` = 插入位置（None = 尾部，双击路径）。
/// 已存在同名同源 → 状态栏提示不动。加入后装列表视图隐藏该项（旧工具 MOVE 观感，
/// 由 filter_indices 的 applied 排除实现，目录数据不动 → 回来时回到原扫描位置）。
fn add_installed(app: &AppWindow, st: &State, installed_idx: usize, at: Option<usize>) {
    let mut dup = false;
    let mut inserted_at = None;
    let ft = app.get_installed_filter().to_string();
    {
        let mut s = st_lock(st);
        if s.applying || !matches!(s.dialog, Dialog::None) {
            return;
        }
        let Some(m) = s.installed.get(installed_idx).cloned() else {
            return;
        };
        let a = applied_from_mod(&m);
        if s.applied
            .iter()
            .any(|r| r.name == a.name && r.source == a.source)
        {
            dup = true;
        } else {
            let pos = at.unwrap_or(s.applied.len()).min(s.applied.len());
            s.applied.insert(pos, AppliedRow::from_applied(&a));
            remark(&mut s);
            s.view = filter_indices(&s.installed, &ft, &s.applied);
            inserted_at = Some(pos as i32);
        }
    }
    if dup {
        set_status(app, "该模组已在已应用列表中。", 3);
        return;
    }
    refresh_applied(app, st);
    refresh_installed(app, st);
    app.set_current_installed(-1);
    if let Some(p) = inserted_at {
        app.set_current_applied(p);
    }
}

/// 应用列表内拖拽重排：`from` 移到插入位 `to`（旧 DropHandler：源在落点前则回退一格）。
fn reorder_applied(app: &AppWindow, st: &State, from: usize, to: usize) {
    let new_pos;
    {
        let mut s = st_lock(st);
        if s.applying || !matches!(s.dialog, Dialog::None) {
            return;
        }
        if from >= s.applied.len() {
            return;
        }
        let mut t = to.min(s.applied.len());
        if t == from {
            return; // 原地放下
        }
        let row = s.applied.remove(from);
        if t > from {
            t -= 1;
        }
        s.applied.insert(t, row);
        remark(&mut s);
        new_pos = t as i32;
    }
    refresh_applied(app, st);
    app.set_current_applied(new_pos);
}

// ============================================================ 列表刷新（仅 UI 线程）

#[cfg(test)]
mod drag_tests {
    use super::{APPLIED_ROW_PX, drop_index};

    #[test]
    fn drop_index_rounds_to_nearest_row_edge() {
        let lv_y = 100.0;
        // 指针在第 1 行中部（100+23）→ 插 0 前还是 1 前的分界在 23px
        assert_eq!(drop_index(lv_y + 22.9, lv_y, 0.0, 5), Some(0));
        assert_eq!(drop_index(lv_y + 23.1, lv_y, 0.0, 5), Some(1));
        assert_eq!(
            drop_index(lv_y + APPLIED_ROW_PX * 2.5, lv_y, 0.0, 5),
            Some(3)
        );
    }

    #[test]
    fn drop_index_content_offset_and_clamp() {
        let lv_y = 100.0;
        // 向下滚动 46px（content-y=-46）：指针在列表顶 = 内容坐标 46 = 第 1 行中
        assert_eq!(drop_index(lv_y, lv_y, -APPLIED_ROW_PX, 5), Some(1));
        // 列表上方/下方 → 收拢到 0 / len
        assert_eq!(drop_index(lv_y - 500.0, lv_y, 0.0, 5), Some(0));
        assert_eq!(drop_index(lv_y + 5000.0, lv_y, 0.0, 5), Some(5));
        // 空列表无落点
        assert_eq!(drop_index(lv_y, lv_y, 0.0, 0), None);
    }
}

fn refresh_estates(app: &AppWindow, st: &State) {
    let (rows, count_text, hint, first_run) = {
        let s = st_lock(st);
        let rows: Vec<EstateItem> = s
            .estates
            .iter()
            .map(|e| EstateItem {
                profile: e.estate.profile.clone().into(),
                name: e.label.clone().into(),
                count_text: format!("{} 个模组", e.count).into(),
                ok: e.ok,
                error: e.error.clone().into(),
            })
            .collect();
        let first_run = s.cfg.roots.save_root.is_none();
        let count_text = if first_run {
            "未配置".to_string()
        } else {
            format!("{} 个存档", s.estates.len())
        };
        let hint = if first_run {
            "未配置存档根目录：点 ⚙ 设置 选择"
        } else if s.estates.is_empty() {
            "存档根下未发现任何 estate（含 persist.game.json 的子目录）"
        } else {
            ""
        }
        .to_string();
        (rows, count_text, hint, first_run)
    };
    app.set_estate_model(ModelRc::new(VecModel::from_iter(rows)));
    app.set_estate_count_text(count_text.into());
    app.set_estate_hint(hint.into());
    app.set_first_run(first_run);
    refresh_estate_detail(app, st); // 详情区随同一份 current/estates 状态刷新
}

/// 左列底部详情区（design-spec）：显示已载入档的模式 / 模组数 / profile / DLC chips；
/// 无选中（未选档 / 重扫后）→ 占位态。ok=false → 名字照显 + 错误缘由红字。
fn refresh_estate_detail(app: &AppWindow, st: &State) {
    let snap = {
        let s = st_lock(st);
        s.current.and_then(|i| s.estates.get(i)).map(|e| {
            (
                e.label.clone(),
                e.mode.clone(),
                e.count,
                e.ok,
                e.error.clone(),
                e.dlcs.clone(),
                e.estate.profile.clone(),
            )
        })
    };
    let Some((label, mode, count, ok, error, dlcs, profile)) = snap else {
        app.set_detail_visible(false);
        return;
    };
    app.set_detail_visible(true);
    app.set_detail_name(label.into());
    app.set_detail_meta(
        if ok {
            format!("{mode} · {count} 个模组 · {profile}")
        } else {
            String::new()
        }
        .into(),
    );
    app.set_detail_error(if ok { String::new() } else { error }.into());
    // DLC chips 双列：奇偶拆分成两列（slint 侧不做表格表达式）。
    let a: Vec<SharedString> = dlcs
        .iter()
        .step_by(2)
        .cloned()
        .map(SharedString::from)
        .collect();
    let b: Vec<SharedString> = dlcs
        .iter()
        .skip(1)
        .step_by(2)
        .cloned()
        .map(SharedString::from)
        .collect();
    app.set_detail_dlcs_a(ModelRc::new(VecModel::from_iter(a)));
    app.set_detail_dlcs_b(ModelRc::new(VecModel::from_iter(b)));
}

// 缩略图懒解码（UI 线程零阻塞）：slint::Image 非 Send（不能进 Shared、不能跨线程），
// 解码在工作线程池做（ICON_WORKERS 个，队列互斥，同路径去重保证不会重复解）。
// 装 mod 列表可达数百行（分发给别人用），解码由视口驱动：
// ListView changed content-y → 只把可见行入队（队首插队），滚到哪解到哪；
// 其余列表（应用/存档）短，刷新时仍全量预解码。完成后 upgrade_in_event_loop
// 回 UI 线程建图，并按 icon-path 就地 set_row_data —— 禁止整模型替换：
// set_installed_model 会重置 ListView 滚动位置。
// 缓存按路径只增；解码失败也缓存空图（占位符），避免每次刷新重试风暴。
// 重扫同名路径图标变更属极罕见边缘，接受。
thread_local! {
    static ICON_CACHE: std::cell::RefCell<std::collections::HashMap<PathBuf, slint::Image>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static ICON_PENDING: std::cell::RefCell<std::collections::HashSet<PathBuf>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    static INSTALLED_MODEL: std::cell::RefCell<Option<std::rc::Rc<VecModel<InstalledItem>>>> =
        const { std::cell::RefCell::new(None) };
    static APPLIED_MODEL: std::cell::RefCell<Option<std::rc::Rc<VecModel<AppliedItem>>>> =
        const { std::cell::RefCell::new(None) };
}

/// 装列表行高（appwindow.slint ModRow height: 46px）与视口请求的兜底行数。
const INSTALLED_ROW_PX: f32 = 46.0;
const VIEWPORT_SLACK_ROWS: usize = 8;
/// 解码线程数：4 并发把首屏 ~18 张的串行解码压到约 1/4 时长。
const ICON_WORKERS: usize = 4;

/// 解码任务队列：多工作线程 + 优先级 FIFO（front=true 插队首=可见行优先）。
/// spawn 失败（极端）→ 永远占位符，不影响功能。
static ICON_Q: std::sync::Mutex<std::collections::VecDeque<(PathBuf, Weak<AppWindow>)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static ICON_CV: std::sync::Condvar = std::sync::Condvar::new();
static ICON_SPAWN: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// 把队列中满足 key 的首个任务挪出（供重新插队）；找不到返回 None。
fn queue_take_by<T>(q: &mut std::collections::VecDeque<T>, key: impl Fn(&T) -> bool) -> Option<T> {
    q.iter().position(key).and_then(|pos| q.remove(pos))
}

/// 可视行区间（含缓冲）：offset/行高取下界，(offset+视高)/行高取上界再留 slack。
/// 返回 [first, last)，空视口或越界返回空区间。
fn visible_rows(offset_px: f32, view_h_px: f32, len: usize) -> (usize, usize) {
    if len == 0 || offset_px < 0.0 || view_h_px <= 0.0 {
        return (0, 0);
    }
    let first = ((offset_px / INSTALLED_ROW_PX).floor() as usize).min(len);
    if first >= len {
        return (0, 0);
    }
    let last = (((offset_px + view_h_px) / INSTALLED_ROW_PX).ceil() as usize + VIEWPORT_SLACK_ROWS)
        .clamp(first + 1, len);
    (first, last)
}

/// 入队一个解码任务（去重；front=true 提到队首）。
fn icon_enqueue(path: &std::path::Path, weak: &Weak<AppWindow>, front: bool) {
    let fresh = ICON_PENDING.with(|p| p.borrow_mut().insert(path.to_path_buf()));
    let mut q = ICON_Q.lock().expect("icon queue");
    if !fresh {
        // 已在队列：仅需提前时挪到队首（保留原 weak，避免重复任务）。
        if front && let Some(job) = queue_take_by(&mut q, |(p, _)| p == path) {
            q.push_front(job);
            ICON_CV.notify_one();
        }
        return;
    }
    let job = (path.to_path_buf(), weak.clone());
    if front {
        q.push_front(job);
    } else {
        q.push_back(job);
    }
    drop(q);
    ICON_CV.notify_one();
}

fn ensure_icon_worker() {
    ICON_SPAWN.get_or_init(|| {
        for _ in 0..ICON_WORKERS {
            let _ = std::thread::Builder::new()
                .name("icon-decode".into())
                .spawn(|| {
                    loop {
                        let job = {
                            let mut q = ICON_Q.lock().expect("icon queue");
                            while q.is_empty() {
                                q = ICON_CV.wait(q).expect("icon queue");
                            }
                            q.pop_front().expect("non-empty")
                        };
                        let (p, weak) = job;
                        // 按内容嗅探解码（不按扩展名）：工坊常见 jpeg/webp 冒名 .png
                        // （如 2946617425 实为 JFIF）；image crate 与 slint 内部依赖同版本。
                        let pixels = decode_icon(&p);
                        let _ = weak.upgrade_in_event_loop(move |app| {
                            let img = pixels.map(slint::Image::from_rgba8).unwrap_or_default();
                            ICON_CACHE.with(|c| c.borrow_mut().insert(p.clone(), img));
                            ICON_PENDING.with(|c| {
                                c.borrow_mut().remove(&p);
                            });
                            refill_icons(&app, &p);
                        });
                    }
                });
        }
    });
}

/// 只查缓存，不派活（装列表行构造用——解码由视口回调驱动）。
fn icon_cached(path: Option<&std::path::Path>) -> slint::Image {
    let Some(path) = path else {
        return slint::Image::default();
    };
    ICON_CACHE
        .with(|c| c.borrow().get(path).cloned())
        .unwrap_or_default()
}

/// 缓存命中即回；未命中入队尾（短列表全量预解码路径）。
fn icon_for(path: Option<&std::path::Path>, weak: &Weak<AppWindow>) -> slint::Image {
    let Some(path) = path else {
        return slint::Image::default();
    };
    if let Some(img) = ICON_CACHE.with(|c| c.borrow().get(path).cloned()) {
        return img;
    }
    ensure_icon_worker();
    icon_enqueue(path, weak, false);
    slint::Image::default()
}

/// 装列表视口回调（slint changed content-y）：把可视行（含缓冲）提到队首解码。
fn request_visible_icons(app: &AppWindow, offset_px: f32, view_h_px: f32) {
    ensure_icon_worker();
    let weak = app.as_weak();
    let paths: Vec<PathBuf> = INSTALLED_MODEL.with(|m| {
        let g = m.borrow();
        let Some(m) = g.as_ref() else {
            return Vec::new();
        };
        let (a, b) = visible_rows(offset_px, view_h_px, m.row_count());
        (a..b)
            .filter_map(|i| m.row_data(i))
            .filter(|it| it.icon.size().width == 0 && !it.icon_path.is_empty())
            .map(|it| PathBuf::from(it.icon_path.as_str()))
            .collect()
    });
    for p in paths {
        icon_enqueue(&p, &weak, true);
    }
}

/// 后台线程解码：读文件 + 内容嗅探 + 降采样 + 像素缓冲（Send，可过线程）。
/// 缓存里只留 64px 缩略图（瓦片 30px 的 2x HiDPI 余量）：工坊图常为 1024px+
/// （RGBA8 一张 4MB+），不降采样 60 个常驻缓存 ≈ 200MB+。
fn decode_icon(path: &std::path::Path) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    use std::io::Cursor;
    let data = std::fs::read(path).ok()?;
    let decoded = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let rgba = image::imageops::thumbnail(&decoded.to_rgba8(), 64, 64);
    let (width, height) = rgba.dimensions();
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buf.make_mut_bytes().copy_from_slice(rgba.as_raw());
    Some(buf)
}

/// 某图标解码完成：就地回填两列表中引用该路径、且仍显示占位符的行。
/// （UI 线程内执行；改 icon-path 不在 UI 绑定里，纯匹配键。）
fn refill_icons(app: &AppWindow, path: &std::path::Path) {
    let img = ICON_CACHE
        .with(|c| c.borrow().get(path).cloned())
        .unwrap_or_default();
    INSTALLED_MODEL.with(|m| {
        let g = m.borrow();
        let Some(m) = g.as_ref() else { return };
        for i in 0..m.row_count() {
            let Some(item) = m.row_data(i) else { continue };
            if item.icon.size().width == 0
                && !item.icon_path.is_empty()
                && std::path::Path::new(item.icon_path.as_str()) == path
            {
                let mut item = item;
                item.icon = img.clone();
                m.set_row_data(i, item);
            }
        }
    });
    APPLIED_MODEL.with(|m| {
        let g = m.borrow();
        let Some(m) = g.as_ref() else { return };
        for i in 0..m.row_count() {
            let Some(item) = m.row_data(i) else { continue };
            if item.icon.size().width == 0
                && !item.icon_path.is_empty()
                && std::path::Path::new(item.icon_path.as_str()) == path
            {
                let mut item = item;
                item.icon = img.clone();
                m.set_row_data(i, item);
            }
        }
    });
    let _ = app; // 保留签名对称（upgrade_in_event_loop 提供 app；当前更新只走模型）
}

fn refresh_installed(app: &AppWindow, st: &State) {
    let (rows, count_text) = {
        let s = st_lock(st);
        let rows: Vec<InstalledItem> = s
            .view
            .iter()
            .filter_map(|&i| s.installed.get(i))
            .map(|m| {
                // 已在应用列表的项由 s.view 排除（旧工具 MOVE 观感），这里不再打 ✓。
                // mod 目录（悬停提示 + 图标钮打开）：配置根 + dir_name；已不存在 → 空。
                let dir_root = match m.source {
                    ModSource::Local => s.cfg.roots.local_mods_dir.clone(),
                    ModSource::Steam => s.cfg.roots.workshop_dir.clone(),
                };
                let dir_path = dir_root.map(|r| r.join(&m.dir_name)).filter(|p| p.is_dir());
                InstalledItem {
                    title: display_title(m).into(),
                    dir_name: m.dir_name.clone().into(),
                    source_cn: source_cn(m.source).into(),
                    icon: icon_cached(m.icon_path.as_deref()), // 解码由视口回调驱动
                    icon_path: m
                        .icon_path
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned().into())
                        .unwrap_or_default(),
                    dir_path: dir_path
                        .map(|p| p.to_string_lossy().into_owned().into())
                        .unwrap_or_default(),
                }
            })
            .collect();
        let count_text = format!("{} / {}", s.view.len(), s.installed.len());
        (rows, count_text)
    };
    // 保留 VecModel 句柄：懒解码完成后按行就地回填（refill_icons），
    // 整模型替换（set_installed_model）会重置 ListView 滚动位置。
    let model = std::rc::Rc::new(VecModel::from_iter(rows));
    INSTALLED_MODEL.with(|m| *m.borrow_mut() = Some(std::rc::Rc::clone(&model)));
    app.set_installed_model(ModelRc::from(model));
    app.set_installed_count_text(count_text.into());
    // 首屏视口解码（content-y 未滚动不触发 changed）：列表高 ≈ 窗高-页头区，slack 兜余量。
    let view_h = app.window().size().height as f32 / app.window().scale_factor() - 160.0;
    request_visible_icons(app, 0.0, view_h.max(300.0));
}

fn refresh_applied(app: &AppWindow, st: &State) {
    let weak = app.as_weak();
    let (rows, count_text, dirty) = {
        let s = st_lock(st);
        // applied 行 → 已安装 mod：同源同名才取（本地比 title / 工坊比 dir_name，
        // 与 mark_present 同口径）；未命中（mod 已删除）→ 无缩略图/无目录钮。
        let matched = |r: &AppliedRow| -> Option<ModInfo> {
            s.installed
                .iter()
                .find(|m| {
                    m.source == r.source
                        && match m.source {
                            ModSource::Local => m.title == r.name,
                            ModSource::Steam => m.dir_name == r.name,
                        }
                })
                .cloned()
        };
        let dir_path_of = |m: &ModInfo| -> Option<PathBuf> {
            let root = match m.source {
                ModSource::Local => s.cfg.roots.local_mods_dir.clone(),
                ModSource::Steam => s.cfg.roots.workshop_dir.clone(),
            };
            root.map(|r| r.join(&m.dir_name)).filter(|p| p.is_dir())
        };
        let rows: Vec<AppliedItem> = s
            .applied
            .iter()
            .map(|r| {
                let mm = matched(r);
                let ip = mm.as_ref().and_then(|m| m.icon_path.clone());
                let dp = mm.as_ref().and_then(dir_path_of);
                AppliedItem {
                    name: r.name.clone().into(),
                    title: mm
                        .as_ref()
                        .map(display_title)
                        .unwrap_or_else(|| r.name.clone())
                        .into(),
                    source_cn: source_cn(r.source).into(),
                    present: r.present,
                    icon: icon_for(ip.as_deref(), &weak),
                    icon_path: ip
                        .map(|p| p.to_string_lossy().into_owned().into())
                        .unwrap_or_default(),
                    dir_path: dp
                        .map(|p| p.to_string_lossy().into_owned().into())
                        .unwrap_or_default(),
                }
            })
            .collect();
        (
            rows,
            format!("{} 项", s.applied.len()),
            is_dirty(&s.baseline, &s.applied),
        )
    };
    // 句柄保留理由同 refresh_installed（懒解码就地回填）。
    let model = std::rc::Rc::new(VecModel::from_iter(rows));
    APPLIED_MODEL.with(|m| *m.borrow_mut() = Some(std::rc::Rc::clone(&model)));
    app.set_applied_model(ModelRc::from(model));
    app.set_applied_count_text(count_text.into());
    app.set_dirty(dirty);
    // 缺失态（design-spec 状态④）：⚠ 行 + 头部胶囊 + 状态条共用这一个计数。
    let missing = {
        let s = st_lock(st);
        s.applied.iter().filter(|r| !r.present).count()
    };
    app.set_missing_count(missing as i32);
}

fn refresh_presets(app: &AppWindow, st: &State) {
    let (rows, has) = {
        let s = st_lock(st);
        let rows: Vec<PresetItem> = s
            .presets
            .presets
            .iter()
            .map(|(k, v)| PresetItem {
                name: k.clone().into(),
                count_text: format!("{} 条", v.len()).into(),
            })
            .collect();
        (rows, !s.presets.presets.is_empty())
    };
    app.set_preset_model(ModelRc::new(VecModel::from_iter(rows)));
    app.set_has_presets(has);
}

fn sync_selection(app: &AppWindow, st: &State) {
    let cur = st_lock(st).current;
    app.set_current_estate(cur.map(|i| i as i32).unwrap_or(-1));
}

/// 缺失清单对话框内容：数据 = applied 里 !present 的行；过滤框只影响显示。
/// 「全部移除」固定作用于全量缺失项（design-spec：过滤不改变删除范围，防误删收窄集）。
fn refresh_missing_model(app: &AppWindow, st: &State) {
    let query = app.get_missing_filter().to_string();
    let (rows, total) = {
        let s = st_lock(st);
        let q = query.to_lowercase();
        let missing: Vec<&AppliedRow> = s.applied.iter().filter(|r| !r.present).collect();
        let total = missing.len();
        // 标题口径与 refresh_applied 一致（同源同名匹配 installed，失败回退 name）。
        // 缺失行通常匹配不到 → 显示存档文件里的原名（工坊=数字 id，与旧行为一致）。
        let title_for = |r: &AppliedRow| -> String {
            s.installed
                .iter()
                .find(|m| match (r.source, m.source) {
                    (ModSource::Steam, ModSource::Steam) => m.dir_name == r.name,
                    (ModSource::Local, ModSource::Local) => m.title == r.name,
                    _ => false,
                })
                .map(display_title)
                .unwrap_or_else(|| r.name.clone())
        };
        let rows: Vec<MissingItem> = missing
            .into_iter()
            .filter(|r| {
                q.is_empty()
                    || r.name.to_lowercase().contains(&q)
                    || title_for(r).to_lowercase().contains(&q)
            })
            .map(|r| MissingItem {
                name: title_for(r).into(),
                source_cn: source_cn(r.source).into(),
                reason: "本地/工坊目录中已不存在".into(),
            })
            .collect();
        (rows, total)
    };
    let n = rows.len();
    app.set_missing_model(ModelRc::new(VecModel::from_iter(rows)));
    app.set_missing_filtered_text(
        if n == total {
            format!("共 {total} 项")
        } else {
            format!("匹配 {n} / {total} 项")
        }
        .into(),
    );
}

fn set_status(app: &AppWindow, text: &str, kind: i32) {
    app.set_status_text(text.into());
    app.set_status_kind(kind);
}

// ============================================================ 存档载入 / 重扫

fn load_estate(app: &AppWindow, st: &State, idx: usize) {
    let (persist, label) = {
        let s = st_lock(st);
        match s.estates.get(idx) {
            Some(e) => (e.estate.persist_path.clone(), e.label.clone()),
            None => return,
        }
    };

    // persist 路径只来自 discover_estates（GC3 白名单收口，ruling 2）。
    let loaded = EstateFile::open(&persist).and_then(|f| f.applied_mods().map(|mods| (f, mods)));

    match loaded {
        Ok((_file, mods)) => {
            {
                let mut s = st_lock(st);
                s.applied = mods.iter().map(AppliedRow::from_applied).collect();
                remark(&mut s);
                s.baseline = mods;
                s.current = Some(idx);
            }
            set_status(app, &format!("已选择存档「{label}」。"), 0);
        }
        Err(e) => {
            // 旧工具：档仍选中，但内容不可用 → 披露后选中空态。
            {
                let mut s = st_lock(st);
                s.applied.clear();
                s.baseline.clear();
                s.current = Some(idx);
            }
            show_error(
                app,
                st,
                format!("读取存档「{label}」失败：{}", save_error_cn(&e)),
            );
        }
    }
    // 载入后按新 baseline 重过滤装列表（旧工具观感：档内已有 mod 不出现在可用列表）。
    {
        let ft = app.get_installed_filter().to_string();
        let mut s = st_lock(st);
        s.view = filter_indices(&s.installed, &ft, &s.applied);
    }
    sync_selection(app, st);
    refresh_estate_detail(app, st); // 已载入档变化 → 详情区联动
    refresh_applied(app, st);
    refresh_installed(app, st);
    app.set_current_applied(-1);
    app.set_current_installed(-1);
}

fn rescan(app: &AppWindow, st: &State) {
    set_status(app, "正在扫描…", 0);
    let (save_root, local, workshop) = {
        let s = st_lock(st);
        (
            s.cfg.roots.save_root.clone(),
            s.cfg.roots.local_mods_dir.clone(),
            s.cfg.roots.workshop_dir.clone(),
        )
    };
    let weak = app.as_weak();
    let st2 = st.clone();
    let filter_text = app.get_installed_filter().to_string();

    std::thread::spawn(move || {
        // estate 发现 + 逐档元数据（读 persist：estate 名 / 模式 / 模组数）。
        let (estate_list, estate_err): (Vec<EstateData>, Option<String>) = match &save_root {
            None => (Vec::new(), None),
            Some(root) => match discover_estates(root) {
                Ok(list) => {
                    let v = list
                        .into_iter()
                        .map(|e| {
                            let meta = e.load().and_then(|f| {
                                f.applied_mods().and_then(|mods| {
                                    f.dlc_entries().map(|dlc| {
                                        (
                                            f.estate_name().map(str::to_string),
                                            f.game_mode().map(str::to_string),
                                            mods.len(),
                                            dlc,
                                        )
                                    })
                                })
                            });
                            match meta {
                                Ok((name, mode, count, dlc)) => EstateData {
                                    label: name.unwrap_or_else(|| e.profile.clone()),
                                    mode: game_mode_cn(mode.as_deref()),
                                    dlcs: dlc_names(&dlc),
                                    count,
                                    ok: true,
                                    error: String::new(),
                                    estate: e,
                                },
                                Err(err) => EstateData {
                                    label: e.profile.clone(),
                                    mode: game_mode_cn(None),
                                    dlcs: Vec::new(),
                                    count: 0,
                                    ok: false,
                                    error: save_error_cn(&err),
                                    estate: e,
                                },
                            }
                        })
                        .collect();
                    (v, None)
                }
                Err(e) => (Vec::new(), Some(save_error_cn(&e))),
            },
        };

        // 已安装扫描：任一根缺失 → 跳过该侧（旧 BaseScan 的 Directory.Exists 语义）。
        let mut mods: Vec<ModInfo> = Vec::new();
        let mut failed: Vec<PathBuf> = Vec::new();
        if let Some(l) = &local {
            let r = scan_mod_dir(l, ModSource::Local);
            mods.extend(r.mods);
            failed.extend(r.failed_dirs);
        }
        if let Some(w) = &workshop {
            let r = scan_mod_dir(w, ModSource::Steam);
            mods.extend(r.mods);
            failed.extend(r.failed_dirs);
        }

        let _ = weak.upgrade_in_event_loop(move |app| {
            {
                let mut s = st_lock(&st2);
                // 用户规则（2026-11-04 定稿）：重扫/首启 = 未选存档态 —— 选中、已应用、
                // 基线全清。旧版的选中保留与脏态保留删除：重扫通常意味着根目录配置
                // 可能已变，旧引用与新扫描结果不再对得上，保留只会制造错位。
                s.current = None;
                s.applied.clear();
                s.baseline.clear();
                s.estates = estate_list;
                s.installed = mods;
                s.view = filter_indices(&s.installed, &filter_text, &s.applied);
                remark(&mut s);
            }
            refresh_estates(&app, &st2); // 内部同时刷新左列详情区
            refresh_installed(&app, &st2);
            refresh_applied(&app, &st2);
            sync_selection(&app, &st2);
            app.set_current_installed(-1);
            app.set_current_applied(-1);

            // 扫描失败目录（非阻塞披露）。
            let failed_count = failed.len();
            if failed_count > 0 {
                let names: Vec<SharedString> = failed
                    .iter()
                    .map(|p| SharedString::from(p.display().to_string()))
                    .collect();
                app.set_failed_model(ModelRc::new(VecModel::from_iter(names)));
            } else {
                app.set_failed_model(ModelRc::new(VecModel::<SharedString>::default()));
                app.set_show_failed(false);
            }
            app.set_failed_count(failed_count as i32);
            app.set_failed_summary_text(format!("⚠ {} 个目录扫描失败", failed_count).into());

            if let Some(msg) = estate_err {
                show_error(&app, &st2, format!("扫描存档根失败：{msg}"));
            } else {
                set_status(&app, "扫描完成。", 0);
            }
            refresh_game_state(app.as_weak(), st_lock(&st2).cfg.hotapply);
        });
    });
}

fn refresh_game_state(weak: Weak<AppWindow>, hotapply: bool) {
    std::thread::spawn(move || {
        let running = game_running();
        // B 路线：UI 与 DLL 零通信，「引擎接入」概念退役；守卫在应用动作时
        // 现场挂载。状态栏只报进程存活。
        let text: &'static str = match (running, hotapply) {
            (true, true) => "🎮 游戏运行中 · 应用时自动挂守卫",
            (true, false) => "🎮 游戏运行中 · 热修改已关闭",
            (false, _) => "游戏未运行",
        };
        let _ = weak.upgrade_in_event_loop(move |app| {
            app.set_game_state_text(text.into());
            // 引擎就绪态（kind 1）已不存在；仅回收历史 1 值，不打扰
            // 2 应用中 / 3 结果态。
            if !app.get_applying() && app.get_status_kind() == 1 {
                app.set_status_kind(0);
            }
        });
    });
}

fn maybe_show_next_error(app: &AppWindow, st: &State) {
    let next = st_lock(st).pending_errors.pop();
    if let Some(msg) = next {
        show_error(app, st, msg);
    }
}

fn show_error(app: &AppWindow, st: &State, msg: String) {
    open_dialog(app, st, Dialog::Error, "出错了", &msg, vec!["确定"]);
}

fn open_dialog(
    app: &AppWindow,
    st: &State,
    dlg: Dialog,
    title: &str,
    message: &str,
    buttons: Vec<&str>,
) {
    let kind = dlg.kind();
    {
        let mut s = st_lock(st);
        s.dialog = dlg;
    }
    app.set_dlg_kind(kind);
    app.set_dlg_title(title.into());
    app.set_dlg_message(message.into());
    // 末位 = 取消钮：摘出单独渲染（恒可点、居右），dlg-buttons 只留动作钮；
    // 取消钮回调下标 = dlg-buttons.length（与 Esc/回车=取消 的约定一致）。
    let mut btns = buttons;
    let cancel = btns.pop().unwrap_or("");
    app.set_dlg_cancel_label(cancel.into());
    app.set_dlg_buttons(ModelRc::new(VecModel::from_iter(
        btns.into_iter().map(SharedString::from),
    )));
    if kind == 3 {
        app.set_dlg_input("".into());
    }
    if kind == 4 || kind == 8 {
        app.set_dlg_list_current(-1);
    }
}

fn close_dialog(app: &AppWindow, st: &State) {
    {
        let mut s = st_lock(st);
        s.dialog = Dialog::None;
        s.preset_rename_from = None;
    }
    app.set_dlg_kind(0);
}

// ============================================================ 对话层按钮分发

fn on_dlg_button(app: &AppWindow, st: &State, idx: i32) {
    let dlg = st_lock(st).dialog.clone();
    match dlg {
        Dialog::None => {}
        Dialog::ConfirmSwitch { target } => {
            close_dialog(app, st);
            if idx == 0 {
                load_estate(app, st, target);
            }
        }
        Dialog::ConfirmExit => {
            close_dialog(app, st);
            if idx == 0 {
                // 确认退出：hide 触发 close_requested 已守过脏态，此处直接放行。
                let _ = app.hide();
            }
        }
        Dialog::BackupFail { estate } => {
            close_dialog(app, st);
            match idx {
                0 => start_apply(app, st, estate, false),
                1 => start_apply(app, st, estate, true),
                _ => set_status(app, "已取消应用（未做任何修改）。", 3),
            }
        }
        Dialog::ConfirmPresetOverwrite { name } => {
            close_dialog(app, st);
            if idx == 0 {
                save_preset_named(app, st, &name, true);
            }
        }
        Dialog::PresetSave => on_preset_save_button(app, st, idx),
        Dialog::PresetLoad => on_preset_load_button(app, st, idx),
        Dialog::PresetManage => on_preset_manage_button(app, st, idx),
        Dialog::Settings => {
            if idx == 0 {
                apply_settings(app, st);
            } else {
                close_dialog(app, st);
            }
        }
        Dialog::Error => {
            close_dialog(app, st);
            maybe_show_next_error(app, st);
        }
        Dialog::Missing => {
            // 0 = 全部移除（固定作用域 = 全部缺失项，与当前过滤框无关，防误删收窄集）；
            // 末位（Esc/关闭）= 仅关闭。
            close_dialog(app, st);
            if idx == 0 {
                {
                    let mut s = st_lock(st);
                    s.applied.retain(|r| r.present);
                    let ft = app.get_installed_filter().to_string();
                    s.view = filter_indices(&s.installed, &ft, &s.applied);
                }
                refresh_applied(app, st);
                refresh_installed(app, st);
                app.set_current_applied(-1);
                app.set_current_installed(-1);
                set_status(app, "已移除全部缺失模组。", 0);
            }
        }
    }
}

fn on_preset_save_button(app: &AppWindow, st: &State, idx: i32) {
    if idx != 0 {
        close_dialog(app, st);
        return;
    }
    let name = app.get_dlg_input().trim().to_string();
    if name.is_empty() {
        set_status(app, "预设名不能为空。", 3);
        return;
    }
    // 重命名模式（PresetManage 预填旧名进来）：整条搬到新名下，保存失败回滚。
    let rename_from = st_lock(st).preset_rename_from.clone();
    if let Some(old) = rename_from {
        if name == old {
            close_dialog(app, st);
            return;
        }
        if st_lock(st).presets.presets.contains_key(&name) {
            set_status(app, &format!("预设名「{name}」已存在。"), 3);
            return;
        }
        let (presets_after, path) = {
            let mut s = st_lock(st);
            let Some(mods) = s.presets.presets.remove(&old) else {
                close_dialog(app, st);
                return;
            };
            s.presets.presets.insert(name.clone(), mods);
            (s.presets.clone(), s.cfg.presets_path.clone())
        };
        if let Err(e) = presets_after.save(&path) {
            let mut s = st_lock(st);
            let moved = s.presets.presets.remove(&name);
            if let Some(m) = moved {
                s.presets.presets.insert(old.clone(), m);
            }
            drop(s);
            show_error(app, st, format!("预设重命名失败：{}", save_error_cn(&e)));
            return;
        }
        close_dialog(app, st);
        refresh_presets(app, st);
        set_status(app, &format!("已重命名预设「{old}」→「{name}」。"), 3);
        return;
    }
    // 新增模式：输入框只管新增（同名拒绝，覆盖走双击行+确认框）；空名已在上面拦过。
    if st_lock(st).presets.presets.contains_key(&name) {
        set_status(
            app,
            &format!("预设「{name}」已存在；双击列表里该行可覆盖，或换个名字。"),
            3,
        );
        return;
    }
    save_preset_named(app, st, &name, false);
}

/// 把当前已应用列表写入预设 name（新增或覆盖），失败回滚内存并弹窗。
/// close=false（新增）：对话框停留，列表原地刷新 + 输入框清空作为成功反馈；
/// close=true（覆盖确认）：关窗，状态栏提醒。
fn save_preset_named(app: &AppWindow, st: &State, name: &str, close: bool) {
    let (presets, path, prior) = {
        let mut s = st_lock(st);
        let mods: Vec<AppliedMod> = s.applied.iter().map(|r| r.to_applied()).collect();
        // insert 返回被覆盖的旧值 —— 保存失败时按它精确还原（而非整个移除）。
        let prior = s.presets.presets.insert(name.to_string(), mods);
        (s.presets.clone(), s.cfg.presets_path.clone(), prior)
    };
    if let Err(e) = presets.save(&path) {
        // 回滚内存（有旧值还旧值，本来没有则移除）+ 弹窗披露（绝不静默）。
        let mut s = st_lock(st);
        match prior {
            Some(old) => {
                s.presets.presets.insert(name.to_string(), old);
            }
            None => {
                s.presets.presets.remove(name);
            }
        }
        drop(s);
        show_error(app, st, format!("预设保存失败：{}", save_error_cn(&e)));
        return;
    }
    if close {
        close_dialog(app, st);
    } else {
        // 新增：不关窗——列表原地出新行 + 输入框清空，就是「新增成功」的可见反馈。
        app.set_dlg_input("".into());
    }
    refresh_presets(app, st);
    let verb = if prior.is_some() {
        "已覆盖"
    } else {
        "已新增"
    };
    set_status(app, &format!("✅{verb}预设「{name}」。"), 3);
}

/// 保存预设：双击行（行下标 = 预设序）→ 弹覆盖确认框；确认后才真正写。
fn on_preset_overwrite_row(app: &AppWindow, st: &State, idx: i32) {
    let name = st_lock(st)
        .presets
        .presets
        .keys()
        .nth(idx.max(0) as usize)
        .cloned();
    let Some(name) = name else {
        close_dialog(app, st);
        return;
    };
    let count = st_lock(st).applied.len();
    open_dialog(
        app,
        st,
        Dialog::ConfirmPresetOverwrite { name: name.clone() },
        "覆盖预设",
        &format!("将用当前已应用的 {count} 项覆盖预设「{name}」。\n原「{name}」的内容将被替换。"),
        vec!["覆盖", "取消"],
    );
}

fn on_preset_load_button(app: &AppWindow, st: &State, idx: i32) {
    // 底部「载入」（idx 0，作用对象 = 当前行选中）；其余（关闭钮）只收起对话层。
    if idx != 0 {
        close_dialog(app, st);
        return;
    }
    let cur = app.get_dlg_list_current();
    on_preset_load_row(app, st, cur);
}

/// 「应用预设」底部「载入」（行选中下标 = 预设序）。
fn on_preset_load_row(app: &AppWindow, st: &State, idx: i32) {
    let name = st_lock(st)
        .presets
        .presets
        .keys()
        .nth(idx.max(0) as usize)
        .cloned();
    let Some(name) = name else {
        close_dialog(app, st);
        return;
    };
    try_load_preset(app, st, &name);
}

fn on_preset_manage_button(app: &AppWindow, st: &State, idx: i32) {
    // 底部动作（作用对象 = 当前行选中）：0=重命名 1=删除；其余（关闭钮）只收起。
    if idx > 1 {
        close_dialog(app, st);
        return;
    }
    let cur = app.get_dlg_list_current();
    if idx == 0 {
        on_preset_rename_row(app, st, cur);
    } else {
        on_preset_delete_row(app, st, cur);
    }
}

/// 预设管理：底部「重命名」— 复用 PresetSave 输入框对话，预填旧名。
fn on_preset_rename_row(app: &AppWindow, st: &State, idx: i32) {
    let name = st_lock(st)
        .presets
        .presets
        .keys()
        .nth(idx.max(0) as usize)
        .cloned();
    let Some(name) = name else {
        close_dialog(app, st);
        return;
    };
    st_lock(st).preset_rename_from = Some(name.clone());
    open_dialog(
        app,
        st,
        Dialog::PresetSave,
        "重命名预设",
        &format!("把「{name}」改成："),
        vec!["保存", "取消"],
    );
    app.set_dlg_input(name.into());
}

/// 预设管理：底部「删除」（保存失败则回滚 + 弹窗）。
fn on_preset_delete_row(app: &AppWindow, st: &State, idx: i32) {
    let name = st_lock(st)
        .presets
        .presets
        .keys()
        .nth(idx.max(0) as usize)
        .cloned();
    let Some(name) = name else {
        close_dialog(app, st);
        return;
    };
    let (removed, presets_after, path) = {
        let mut s = st_lock(st);
        let removed = s.presets.presets.remove(&name);
        (removed, s.presets.clone(), s.cfg.presets_path.clone())
    };
    if let Err(e) = presets_after.save(&path) {
        if let Some(mods) = removed {
            st_lock(st).presets.presets.insert(name.clone(), mods);
        }
        show_error(app, st, format!("预设删除失败：{}", save_error_cn(&e)));
        return;
    }
    refresh_presets(app, st);
    app.set_dlg_list_current(-1);
    set_status(app, &format!("已删除预设「{name}」。"), 3);
}

/// 载入预设前置条件：已选存档（写入锚点）+ 脏确认（覆盖已应用列表）。
/// 调用方 = 「应用预设」对话框底部「载入」（on_preset_load_button）。
fn try_load_preset(app: &AppWindow, st: &State, name: &str) {
    // 载入预设只替换「已应用」列表、不写盘：同一存档内随时可换，不做脏确认。
    // 脏确认只在切换存档 / 退出时拦（见存档点击与 close_requested）。
    if st_lock(st).current.is_none() {
        set_status(app, "请先在左侧选择一个存档，再载入预设。", 3);
        return;
    }
    load_preset_now(app, st, name);
}

/// 实际载入（前置条件已由调用方守卫：已选存档）。
/// 缺失项保留并标 ⚠，与旧工具「自动剔除缺失」不同 —— 评审裁定 KEEP-AS-IS（更安全）。
fn load_preset_now(app: &AppWindow, st: &State, name: &str) {
    close_dialog(app, st); // 幂等：无论从载预设还是确认框进来都先收起对话层。
    let missing = {
        let mut s = st_lock(st);
        let mods = s.presets.presets.get(name).cloned().unwrap_or_default();
        s.applied = mods.iter().map(AppliedRow::from_applied).collect();
        remark(&mut s);
        s.applied.iter().filter(|r| !r.present).count()
    };
    // 预设载入后按新 applied 重过滤装列表（与档载入同款观感）。
    {
        let ft = app.get_installed_filter().to_string();
        let mut s = st_lock(st);
        s.view = filter_indices(&s.installed, &ft, &s.applied);
    }
    refresh_applied(app, st);
    refresh_installed(app, st);
    app.set_current_applied(-1);
    app.set_current_installed(-1);
    if missing > 0 {
        set_status(
            app,
            &format!("已载入预设「{name}」（{missing} 项在当前仓库中不存在，保留并标 ⚠）。"),
            3,
        );
    } else {
        set_status(app, &format!("已载入预设「{name}」。"), 3);
    }
}

fn apply_settings(app: &AppWindow, st: &State) {
    let new_roots = SaveRoots {
        save_root: non_empty_path(&app.get_cfg_save_root()),
        local_mods_dir: non_empty_path(&app.get_cfg_local_mods()),
        workshop_dir: non_empty_path(&app.get_cfg_workshop()),
    };
    // 候选配置先落盘、成功后才换内存 —— 失败时内存 roots 保持原值（可重试/取消）。
    let (candidate, path, changed) = {
        let s = st_lock(st);
        let mut candidate = s.cfg.clone();
        candidate.roots = new_roots;
        candidate.hotapply = app.get_cfg_hotapply();
        let changed = s.cfg.roots != candidate.roots || s.cfg.hotapply != candidate.hotapply;
        (candidate, s.cfg_path.clone(), changed)
    };
    if let Err(msg) = candidate.save(&path) {
        // 保存失败：弹窗披露（settings 对话被替换为错误对话），内存未动。
        show_error(app, st, format!("保存配置失败：{msg}"));
        return;
    }
    st_lock(st).cfg = candidate;
    close_dialog(app, st);
    if changed {
        set_status(app, "设置已保存，正在重扫…", 0);
        rescan(app, st);
    } else {
        set_status(app, "设置已保存（未变化）。", 0);
    }
    refresh_estates(app, st);
}

// ============================================================ 应用流程（旧 C# 对齐）

/// 备份永远先行（旧 BackupManager.BackupSave）；失败 → 三选一弹窗（ruling：
/// 同秒同名备份 Err 是旧工具 parity，捕获后披露，不崩溃）。
fn start_apply(app: &AppWindow, st: &State, estate_idx: usize, skip_backup: bool) {
    let (persist, dir, label, mods, savebak, hot, save_root) = {
        let mut s = st_lock(st);
        if s.applying {
            return;
        }
        let Some(e) = s.estates.get(estate_idx) else {
            return;
        };
        if !e.ok {
            let msg = format!("存档「{}」当前无法读取，请先重扫。", e.label);
            drop(s);
            show_error(app, st, msg);
            return;
        }
        let persist = e.estate.persist_path.clone();
        let dir = e.estate.dir.clone();
        let label = e.label.clone();
        let mods: Vec<AppliedMod> = s.applied.iter().map(|r| r.to_applied()).collect();
        let savebak = s.savebak.clone();
        let hot = s.cfg.hotapply;
        let save_root = s.cfg.roots.save_root.clone();
        s.applying = true;
        (persist, dir, label, mods, savebak, hot, save_root)
    };
    app.set_applying(true);
    set_status(app, "正在应用…", 2);

    let weak = app.as_weak();
    let st2 = st.clone();
    std::thread::spawn(move || {
        // panic 兜底（终态审查 IMP-2）：worker 内任何 panic 若无人收尾，
        // s.applying 永真 → 状态 pill 卡"应用中"、busy() 拦截一切回调、
        // on_close_requested 拒绝关窗 —— 用户只能杀进程。这里捕获后走
        // 标准错误收尾；存档写入是 tmp+rename 原子写，panic 不留半文件。
        let weak_err = weak.clone();
        let st_err = st2.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply_worker(
                &weak,
                &st2,
                estate_idx,
                skip_backup,
                persist,
                dir,
                label,
                mods,
                savebak,
                hot,
                save_root,
            );
        }));
        if let Err(panic) = outcome {
            let reason = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知".to_string());
            finish_apply(
                &weak_err,
                &st_err,
                estate_idx,
                ApplyMsg::Error(format!(
                    "应用流程发生内部错误（{reason}）——界面已恢复。存档为原子写不会损坏；请重试，若持续出现请反馈日志。"
                )),
            );
        }
    });
}

/// 应用工作线程主体（在 std::thread 里跑；所有游戏/磁盘交互都在这里）。
/// 允许 panic：外层 catch_unwind 会兜底收尾 UI 状态。
/// （参数即 worker 全部输入快照，打包成结构体反而隔一层——放行参数数。）
#[allow(clippy::too_many_arguments)]
fn apply_worker(
    weak: &Weak<AppWindow>,
    st2: &State,
    estate_idx: usize,
    skip_backup: bool,
    persist: std::path::PathBuf,
    dir: std::path::PathBuf,
    label: String,
    mods: Vec<AppliedMod>,
    savebak: std::path::PathBuf,
    hotapply: bool,
    save_root: Option<std::path::PathBuf>,
) {
    let running = game_running();
    if !skip_backup && let Err(e) = backup_estate_dir(&dir, &label, &savebak) {
        finish_apply(
            weak,
            st2,
            estate_idx,
            ApplyMsg::BackupFail(save_error_cn(&e)),
        );
        return;
    }

    // B 路线（v14）：守卫挂不上就不写盘——游戏运行中裸写存档，若本会话游戏
    // 已读过该文件，陈旧 size 缓存会在下次读档时撞断言（E1c 定案）。游戏未
    // 运行则零风险：冷启动读盘拿到的必然是新文件。
    // TOCTOU 补哨：备份 zip 窗口内游戏可能刚启动（启动读 persist 会种下陈旧
    // size 缓存）——写盘前重查进程，已启动同样走挂守卫路径。
    let hot_path = hotapply && (running || game_running());
    if !hot_path {
        let r = apply_offline(&persist, &mods, &ApplyOptions { backup_dir: None });
        let text = if hotapply {
            offline_fallback_text()
        } else if running {
            file_only_game_running_text()
        } else {
            file_only_ok_text()
        };
        finish_apply(weak, st2, estate_idx, offline_msg(r, text));
        return;
    }

    let log_dir = model::exe_dir().join(LOG_DIR_NAME);
    if let Err(e) = ensure_hooked(&log_dir, save_root.as_deref()) {
        finish_apply(
            weak,
            st2,
            estate_idx,
            ApplyMsg::Error(format!(
                "{}。已中止（未写存档）——可重试，或关闭游戏后再应用。",
                e.detail()
            )),
        );
        return;
    }
    // 守卫已挂：离线原子写盘 + 仪式指引（重看开场动画触发 refresh，守卫在
    // size 查询处对齐磁盘真值，游戏读到新内容）。
    let r = apply_offline(&persist, &mods, &ApplyOptions { backup_dir: None });
    let text = hotapply_ritual_text();
    finish_apply(weak, st2, estate_idx, offline_msg(r, text));
}

fn offline_msg(r: save::Result<save::ApplyOutcome>, text: String) -> ApplyMsg {
    match r {
        // 离线写成功 → 存档已落定，committed=true（基线 := 本次列表）。
        Ok(_) => ApplyMsg::Status {
            text,
            committed: true,
        },
        Err(SaveError::Backup { path, reason }) => {
            // 同秒同名 zip 等备份冲突：旧工具 parity 的 Err → 三选一弹窗。
            ApplyMsg::BackupFail(format!("备份失败：{}（{reason}）", path.display()))
        }
        Err(e) => ApplyMsg::Error(format!("写入存档失败：{}", save_error_cn(&e))),
    }
}

fn finish_apply(weak: &Weak<AppWindow>, st: &State, _estate_idx: usize, msg: ApplyMsg) {
    let weak = weak.clone();
    let st2 = st.clone();
    let _ = weak.upgrade_in_event_loop(move |app| {
        {
            let mut s = st_lock(&st2);
            s.applying = false;
        }
        app.set_applying(false);
        match msg {
            ApplyMsg::Status { text, committed } => {
                if committed {
                    let mut s = st_lock(&st2);
                    s.baseline = s.applied.iter().map(|r| r.to_applied()).collect();
                }
                refresh_applied(&app, &st2);
                set_status(&app, &text, 3);
            }
            ApplyMsg::BackupFail(detail) => {
                open_dialog(
                    &app,
                    &st2,
                    Dialog::BackupFail {
                        estate: _estate_idx,
                    },
                    "备份失败",
                    &format!("{detail}\n\n重试备份、跳过备份继续应用，还是取消？"),
                    vec!["重试备份", "跳过备份继续", "取消"],
                );
                set_status(&app, "备份失败。", 3);
            }
            ApplyMsg::Error(detail) => {
                show_error(&app, &st2, detail);
                set_status(&app, "应用失败。", 3);
            }
        }
        refresh_game_state(app.as_weak(), st_lock(&st2).cfg.hotapply);
    });
}

#[cfg(test)]
mod icon_viewport_tests {
    use super::{queue_take_by, visible_rows};

    #[test]
    fn viewport_windows_basic_and_edges() {
        // 46px 行高：首屏 0 偏移、500px 视高 → 0..11 行 + 8 slack → (0,19)
        assert_eq!(visible_rows(0.0, 500.0, 300), (0, 19));
        // 滚到第 100 行（4600px）、460px 视高 → 恰 10 行 + slack
        assert_eq!(visible_rows(4600.0, 460.0, 300), (100, 118));
        // 偏移越界（滚过头/空列表）→ 空区间
        assert_eq!(visible_rows(13_800.0, 500.0, 300), (0, 0));
        assert_eq!(visible_rows(0.0, 500.0, 0), (0, 0));
        assert_eq!(visible_rows(-5.0, 500.0, 300), (0, 0));
        assert_eq!(visible_rows(0.0, 0.0, 300), (0, 0));
    }

    #[test]
    fn queue_take_by_removes_first_match() {
        let mut q = std::collections::VecDeque::from(vec![1, 2, 3, 2]);
        assert_eq!(queue_take_by(&mut q, |&v| v == 2), Some(2));
        assert_eq!(q, std::collections::VecDeque::from(vec![1, 3, 2]));
        assert_eq!(queue_take_by(&mut q, |&v| v == 9), None);
        assert_eq!(q.len(), 3);
    }
}
