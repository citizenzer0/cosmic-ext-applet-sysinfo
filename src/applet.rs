use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use nvml_wrapper::Nvml;
use sysinfo::{Components, CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
use tracing::{debug, trace};

use crate::{
    config::{APP_ID, Flags, SysInfoConfig},
    fl,
};

pub(crate) fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<SysInfo>(Flags::new())
}

struct SysInfo {
    core: cosmic::app::Core,
    popup: Option<cosmic::iced::window::Id>,
    config: SysInfoConfig,
    config_handler: Option<cosmic::cosmic_config::Config>,
    system: System,
    networks: Networks,
    components: Components,
    cpu_usage: f32,
    cpu_temp: Option<f32>,
    ram_usage: u64,
    download_speed: f64,
    upload_speed: f64,
    last_scan: Instant,
    physical_interfaces: Vec<String>,
    ups_temp: String,
    // GPU monitoring (NVIDIA only via NVML)
    nvml: Option<Nvml>,
    gpu_load: Option<u32>,
    gpu_temp: Option<u32>,
    gpu_vram_used: Option<u64>,
    gpu_vram_total: Option<u64>,
}

impl SysInfo {
    fn get_physical_interfaces(config: &SysInfoConfig) -> Vec<String> {
        let mut interfaces = Vec::new();

        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let interface = entry.file_name().into_string().unwrap_or_default();

                if Path::new(&format!("/sys/class/net/{}/device", interface)).exists() {
                    interfaces.push(interface);
                }
            }
        }

        // Apply config filters
        if let Some(included_interfaces) = &config.include_interfaces {
            interfaces.retain(|interface| included_interfaces.contains(interface));
        }
        if let Some(excluded_interface) = &config.exclude_interfaces {
            interfaces.retain(|interface| !excluded_interface.contains(interface));
        }

        interfaces
    }

    fn rescan_physical_interfaces(&mut self) {
        self.physical_interfaces = Self::get_physical_interfaces(&self.config);
        self.last_scan = Instant::now();
    }

    fn update_sysinfo_data(&mut self) {
        // Rescan interfaces every 10 seconds
        if self.last_scan.elapsed() > Duration::from_secs(10) {
            self.rescan_physical_interfaces();
        }

        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_memory(MemoryRefreshKind::nothing().with_ram())
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
        );

        self.cpu_usage = self.system.global_cpu_usage();
        self.ram_usage = if self.config.include_swap_in_ram {
            ((self.system.used_memory() + self.system.used_swap()) * 100)
                / (self.system.total_memory() + self.system.total_swap())
        } else {
            (self.system.used_memory() * 100) / self.system.total_memory()
        };

        // Refresh CPU temperature from components
        // Look for common CPU temperature sensor labels: k10temp (AMD), coretemp (Intel), or "cpu"
        self.components.refresh(true);
        self.cpu_temp = self
            .components
            .iter()
            .find(|c| {
                let label = c.label().to_lowercase();
                label.contains("k10temp")
                    || label.contains("coretemp")
                    || label.contains("cpu")
                    || label.contains("tctl") // AMD Ryzen Tctl
            })
            .and_then(|c| c.temperature());

        self.networks.refresh(true);

        let mut upload = 0;
        let mut download = 0;

        for (name, data) in self.networks.iter() {
            if self.physical_interfaces.contains(name) {
                upload += data.transmitted();
                download += data.received();
            }
        }

        self.upload_speed = (upload as f64) / 1_000_000.0;
        self.download_speed = (download as f64) / 1_000_000.0;
        self.ups_temp = get_ups_temp();

        // Update GPU stats from NVML (NVIDIA only)
        if let Some(ref nvml) = self.nvml {
            if let Ok(device) = nvml.device_by_index(0) {
                // GPU utilization (load)
                self.gpu_load = device.utilization_rates().ok().map(|u| u.gpu);
                // GPU temperature
                self.gpu_temp = device
                    .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                    .ok();
                // GPU VRAM
                if let Ok(mem_info) = device.memory_info() {
                    self.gpu_vram_used = Some(mem_info.used / (1024 * 1024)); // Convert to MB
                    self.gpu_vram_total = Some(mem_info.total / (1024 * 1024)); // Convert to MB
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Message {
    Tick,
    ToggleWindow,
    PopupClosed(cosmic::iced::window::Id),
    ToggleIncludeSwapWithRam(bool),
}

impl cosmic::Application for SysInfo {
    type Flags = Flags;
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;

    const APP_ID: &'static str = APP_ID;

    fn init(
        core: cosmic::app::Core,
        flags: Self::Flags,
    ) -> (Self, cosmic::app::Task<Self::Message>) {
        let config = flags.config;

        let memory_config = if config.include_swap_in_ram {
            MemoryRefreshKind::nothing().with_ram().with_swap()
        } else {
            MemoryRefreshKind::nothing().with_ram()
        };
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_memory(memory_config)
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
        );
        let networks = Networks::new_with_refreshed_list();
        let components = Components::new_with_refreshed_list();

        let last_scan = Instant::now();
        let physical_interfaces = SysInfo::get_physical_interfaces(&config);

        // Initialize NVML for NVIDIA GPU monitoring (may fail on non-NVIDIA systems)
        let nvml = Nvml::init().ok();

        (
            Self {
                core,
                popup: None,
                config,
                config_handler: flags.config_handler,
                system,
                networks,
                components,
                cpu_usage: 0.0,
                cpu_temp: None,
                ram_usage: 0,
                download_speed: 0.00,
                upload_speed: 0.00,
                last_scan,
                physical_interfaces,
                ups_temp: String::from("..."),
                nvml,
                gpu_load: None,
                gpu_temp: None,
                gpu_vram_used: None,
                gpu_vram_total: None,
            },
            cosmic::task::none(),
        )
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn subscription(&self) -> cosmic::iced::Subscription<Message> {
        cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick)
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }

    fn on_close_requested(&self, id: cosmic::iced::window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Message) -> cosmic::app::Task<Self::Message> {
        match message {
            // don't spam the logs with the tick
            Message::Tick => trace!(?message),
            _ => debug!(?message),
        }

        match message {
            Message::Tick => self.update_sysinfo_data(),
            Message::ToggleWindow => {
                if let Some(id) = self.popup.take() {
                    debug!("have popup with id={id}, destroying");

                    return cosmic::iced::platform_specific::shell::commands::popup::destroy_popup(
                        id,
                    );
                } else {
                    debug!("do not have a popup, creating");

                    let new_id = cosmic::iced::window::Id::unique();
                    self.popup.replace(new_id);

                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );

                    return cosmic::iced::platform_specific::shell::commands::popup::get_popup(
                        popup_settings,
                    );
                }
            }
            Message::PopupClosed(id) => {
                self.popup.take_if(|stored_id| stored_id == &id);
            }
            Message::ToggleIncludeSwapWithRam(value) => {
                if let Some(handler) = &self.config_handler
                    && let Err(error) = self.config.set_include_swap_in_ram(handler, value)
                {
                    tracing::error!("{error}")
                }
            }
        }

        cosmic::task::none()
    }

    fn view(&self) -> cosmic::Element<'_, Message> {
        // Format CPU temp (show N/A if unavailable)
        let cpu_temp_str = self
            .cpu_temp
            .map(|t| format!("{:.0}°C", t))
            .unwrap_or_else(|| "N/A".to_string());

        // Format GPU stats
        let gpu_display = match (
            self.gpu_load,
            self.gpu_temp,
            self.gpu_vram_used,
            self.gpu_vram_total,
        ) {
            (Some(load), Some(temp), Some(used), Some(total)) => {
                format!(
                    "GPU {}% {}°C {:.1}/{:.1}GB",
                    load,
                    temp,
                    used as f64 / 1024.0,
                    total as f64 / 1024.0
                )
            }
            _ => "GPU N/A".to_string(),
        };

        let data = {
            cosmic::iced_widget::row![
                cosmic::iced_widget::text(format!("CPU {:.0}% {}", self.cpu_usage, cpu_temp_str)),
                cosmic::iced_widget::text("|"),
                cosmic::iced_widget::text(format!("RAM {}%", self.ram_usage)),
                cosmic::iced_widget::text("|"),
                cosmic::iced_widget::text(format!("UPS {}°C", self.ups_temp)),
                cosmic::iced_widget::text("|"),
                cosmic::iced_widget::text(gpu_display),
                cosmic::iced_widget::text("|"),
                cosmic::iced_widget::text(format!(
                    "↓{:.2}M/s ↑{:.2}M/s",
                    self.download_speed, self.upload_speed
                )),
            ]
            .spacing(4)
        };

        let button = cosmic::widget::button::custom(data)
            .class(cosmic::theme::Button::AppletIcon)
            .on_press_down(Message::ToggleWindow);

        cosmic::widget::autosize::autosize(button, cosmic::widget::Id::unique()).into()
    }

    fn view_window(&self, _id: cosmic::iced::window::Id) -> cosmic::Element<'_, Message> {
        let include_swap_in_ram_toggler = cosmic::iced_widget::row![
            cosmic::widget::text(fl!("include-swap-in-ram-toggle")),
            cosmic::widget::Space::with_width(cosmic::iced::Length::Fill),
            cosmic::widget::toggler(self.config.include_swap_in_ram)
                .on_toggle(Message::ToggleIncludeSwapWithRam),
        ];

        let data = cosmic::iced_widget::column![
            // padding comment to make formatting nicer
            cosmic::applet::padded_control(include_swap_in_ram_toggler)
        ]
        .padding([16, 0]);

        self.core
            .applet
            .popup_container(cosmic::widget::container(data))
            .into()
    }
}

fn get_ups_temp() -> String {
    let output = std::process::Command::new("upsc")
        .arg("eaton@localhost")
        .output();

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if line.contains("ups.temperature") {
                return line.split(':').nth(1).unwrap_or("N/A").trim().to_string();
            }
        }
    }
    "N/A".to_string()
}
