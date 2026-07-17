// main.rs

use anyhow::{Context, Error, Ok, Result};
use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use regex::Regex;
use std::{
	collections::HashMap,
	env,
	ffi::OsStr,
	fs, io,
	path::{Path, PathBuf},
	process::{self, Stdio},
};
use text_template::*;

const NAME: &str = "batlimit";
const UNITPATH: &str = "/etc/systemd/system/batlimit";
const KEYPATH: &str = "/sys/class/power_supply";
const LIMITKEY: &str = "charge_control_end_threshold";
const STARTKEY: &str = "charge_control_start_threshold";
const INTERVAL: u8 = 2;
const TARGETS: [&str; 6] = ["hibernate", "hybrid-sleep", "multi-user", "sleep", "suspend", "suspend-then-hibernate"];
const ENVVAR: &str = "BATLIMIT_BAT";
const CMD: Style = AnsiColor::Magenta.on_default().effects(Effects::BOLD);
const HEAD: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const VALUE: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
const NORMAL: Style = AnsiColor::White.on_default().effects(Effects::BOLD);
const GOOD: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
const STYLE: Styles = Styles::styled()
	.header(GOOD)
	.usage(GOOD)
	.placeholder(AnsiColor::Cyan.on_default())
	.error(ERROR)
	.literal(HEAD)
	.valid(HEAD)
	.invalid(VALUE);

#[derive(Parser)]
#[command(
	version,
	about,
	styles = STYLE,
	after_help = format!("\
	Commands can be abbreviated up to their first letter.\n\
	Root privileges required for: {CMD}limit {NORMAL}& {CMD}clear{NORMAL}, {CMD}persist {NORMAL}& {CMD}unpersist"),
	infer_subcommands(true),
	help_template(
"\
{before-help}{name} {version} - {about}
{usage-heading} {usage}
{all-args}{after-help}
"
))]
struct Cli {
	#[command(subcommand)]
	command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
	/// Print battery info (default command)
	Info,
	/// Set battery charge limit: PERCENT (1..99)
	Limit { percent: u8 },
	/// Clear charge limit
	Clear,
	/// Persist charge limit with systemd: [PERCENT (1..99)]
	Persist { percent: Option<u8> },
	/// Unpersist charge limit: disable and remove systemd services
	Unpersist,
	/// Generate completions: SHELL (bash|elvish|fish|powershell|zsh)
	#[command(long_about = "Generate shell completions. Example:
  batlimit shell bash > _bash
  mkdir -p ~/.local/share/bash_completion/completions
  mv _bash ~/.local/share/bash_completion/completions/batlimit")]
	Shell { shell: Shell },
	/// Output the readme file from the repo
	Readme,
}

struct Battery {
	bat_path: PathBuf,
}
impl Battery {
	fn new() -> Result<Self> {
		let bat_name = env::var(ENVVAR);
		if bat_name.is_ok() && !bat_name.clone().unwrap().is_empty() {
			let bat_path = Path::new(KEYPATH).join(bat_name.unwrap());
			return Ok(Self { bat_path });
		};
		for bat_name in ["BAT0", "BAT1", "BATT", "BATC"] {
			let bat_path = Path::new(KEYPATH).join(bat_name);
			if fs::metadata(&bat_path).is_ok() {
				return Ok(Self { bat_path });
			};
		}
		Err(Error::msg(format!("{ERROR}Battery not found").to_owned()))
	}

	/// Write to a file with sudo, like `echo "$2" |sudo tee "$1" >/dev/null
	fn sudo_write<P: AsRef<Path>, C: AsRef<OsStr>>(path: P, contents: C) -> Result<()> {
		let echo = process::Command::new("echo").arg(contents).stdout(Stdio::piped()).spawn()?;
		process::Command::new("sudo")
			.arg("tee")
			.arg(path.as_ref().as_os_str())
			.stdin(Stdio::from(echo.stdout.ok_or(format!("{ERROR}Nothing piped in by echo")).map_err(Error::msg)?))
			.stdout(Stdio::null())
			.spawn()?
			.wait()?;
		Ok(())
	}

	fn get_limit(&self) -> Result<u8> {
		fs::read_to_string(self.bat_path.join(LIMITKEY))
			.context(format!("Failed to read from {}", self.bat_path.join(LIMITKEY).display()))?
			.trim()
			.parse::<u8>()
			.map_err(|e| Error::msg(format!("{ERROR}Failed to parse battery limit: {VALUE}{e}")))
	}

	fn limit(&self, limit: u8) -> Result<()> {
		if !(1..=99).contains(&limit) {
			return Err(Error::msg(format!("{ERROR}Percent must be a number between 1 and 100")));
		}
		let old_limit = self.get_limit()?;
		Self::sudo_write(self.bat_path.join(LIMITKEY), limit.to_string())?;
		if old_limit == limit {
			println!("Limit unchanged: {VALUE}{limit}%");
		} else if old_limit == 100 {
			println!("Limit set: {VALUE}{limit}%");
		} else {
			println!("Limit changed: {VALUE}{old_limit}% {NORMAL}-> {VALUE}{limit}%");
		}
		Ok(())
	}

	fn clear(&self) -> Result<()> {
		let old_limit = self.get_limit()?;
		if old_limit < 100 {
			Self::sudo_write(self.bat_path.join(LIMITKEY), "100")?;
			println!("Cleared charge limit");
		} else {
			println!("Charge limit already cleared");
		}
		Ok(())
	}

	fn persist(&self, percent: Option<u8>) -> Result<()> {
		let limit = if percent.is_none() { self.get_limit()? } else { percent.unwrap() };
		if !(1..=99).contains(&limit) {
			return Err(Error::msg(format!("{ERROR}Percent must be a number between 1 and 99")));
		}
		let mut values = HashMap::new();
		let startval = limit - INTERVAL;
		let startkey = self.bat_path.join(STARTKEY).display().to_string();
		let startstr = format!("echo {} >{}; ", startval.to_string().as_str(), startkey);
		if fs::exists(self.bat_path.join(STARTKEY)).unwrap() {
			values.insert("start", startstr.as_str());
		} else {
			values.insert("start", "");
		};
		let lim = limit.to_string();
		values.insert("limit", &lim);
		let key = self.bat_path.join(LIMITKEY).display().to_string();
		values.insert("path", &key);
		// Compile-time include from the repo root
		let template = Template::from(include_str!("../unit.service"));
		for target in TARGETS {
			values.insert("target", target);
			let content = template.fill_in(&values).to_string();
			let path = format!("{UNITPATH}-{target}.service");
			Self::sudo_write(&path, content)?;
			let unit = format!("{NAME}-{target}.service");
			process::Command::new("sudo").args(format!("systemctl enable --now {unit} --quiet").split(' ')).spawn()?.wait()?;
		}
		println!("Persist systemd services created and enabled with limit {VALUE}{limit}");
		Ok(())
	}

	fn unpersist(&self) -> Result<()> {
		for target in TARGETS {
			let unit = format!("{NAME}-{target}.service");
			let path = format!("{UNITPATH}-{target}.service");
			if fs::metadata(&path).is_ok() {
				process::Command::new("sudo").args(format!("systemctl disable --now {unit} --quiet").split(' ')).spawn()?.wait()?;
				process::Command::new("sudo").arg("rm").arg(path).spawn()?.wait()?;
			}
		}
		println!("Persist systemd services disabled and removed");
		Ok(())
	}

	fn get_persist(&self) -> Option<u8> {
		let mut percent: u8 = 0;
		for target in TARGETS {
			let path = format!("{UNITPATH}-{target}.service");
			let file = fs::read_to_string(path).ok()?;
			if Some(file.clone()).is_none() {
				return Some(0); // inconclusive: some unit files not present
			}
			let re = Regex::new(format!("(?m)^ExecStart=/bin/sh -c 'echo ([0-9]+) >{KEYPATH}/BAT./{LIMITKEY}'$").as_str()).unwrap();
			let pct = re.captures(&file)?.get(1)?.as_str().parse::<u8>().unwrap();
			if percent == 0 {
				percent = pct;
			} else if percent != pct {
				return Some(0); // inconclusive: different values in unit files
			}
		}
		Some(percent)
	}

	fn info(&self) {
		const INFO: [(&str, &str, &str); 16] = [
			("manufacturer", "Brand", ""),
			("model_name", "Model", ""),
			("technology", "Battery Type", ""),
			("status", "Charge Status", ""),
			("capacity_level", "Battery State", ""),
			("charge_full", "Current Max. Capacity", " Ah"),
			("power_now", "Current Max. Capacity", " Ah"),
			("energy_now", "Current Max. Capacity", " Ah"),
			("energy_full", "Current Max. Capacity", " Ah"),
			("charge_full_design", "Design Max. Capacity", " Ah"),
			("energy_full_design", "Design Max. Capacity", " Ah"),
			("voltage_min_design", "Min. Voltage", " V"),
			("voltage_now", "Current Voltage", " V"),
			("capacity", "Charge Level", "%"),
			(STARTKEY, "Charge Start", "%"),
			(LIMITKEY, "Charge Limit", "%"),
		];
		let info = INFO
			.iter()
			.filter_map(|v| {
				fs::read_to_string(self.bat_path.join(v.0)).ok().map(|value| {
					let value = if v.2.starts_with(' ') {
						let mut val = value.trim().to_owned();
						while val.len() < 7 {
							val.insert(0, '0');
						}
						val.insert(val.len() - 6, '.');
						while val.ends_with('0') {
							val.pop();
						}
						if val.ends_with('.') {
							val.pop();
						}
						val
					} else {
						value.trim().to_owned()
					};
					(v.1, value, v.2)
				})
			})
			.collect::<Vec<_>>();
		let pad_size = info.iter().map(|(file, _, _)| file.len()).max().unwrap_or(0);
		let info_string = info
			.iter()
			.map(|(file, value, unit)| format!("{NORMAL}{file:<pad_size$}  {VALUE}{value}{unit}"))
			.collect::<Vec<_>>()
			.join("\n");
		let path = &self.bat_path.display().to_string();
		let bat = path.split('/').next_back().unwrap();
		println!("{HEAD}[{bat}]");
		if !info_string.is_empty() {
			println!("{info_string}");
		};
		let persiststr = "Persist state";
		let persist = self.get_persist();
		if persist != Some(0) {
			if persist.is_none() {
				println!("{NORMAL}{persiststr:<pad_size$}  {VALUE}NO");
			} else {
				println!("{NORMAL}{persiststr:<pad_size$}  {VALUE}{}%", persist.unwrap());
			}
		} else {
			println!("{NORMAL}{persiststr:<pad_size$}  {VALUE}INCONSISTENT");
		}
		let healthstr = "Health / Capacity";
		let mut cur = String::new();
		let mut des = String::new();
		for triple in &info {
			if triple.0 == INFO[5].1 {
				cur = triple.1.clone();
			};
			if triple.0 == INFO[9].1 {
				des = triple.1.clone();
			};
		}
		if !cur.is_empty() && !des.is_empty() {
			let health = 100.0 * cur.parse::<f32>().unwrap_or(0.0) / des.parse::<f32>().unwrap_or(1.0);
			println!("{NORMAL}{healthstr:<pad_size$}  {VALUE}{:.0}%", health.clamp(0.0, 100.0));
		} else {
			println!("{NORMAL}{healthstr:<pad_size$}  {VALUE}NO INFO");
		};
	}
}

fn main() -> Result<()> {
	let args = Cli::parse();
	let battery = Battery::new()?;
	match args.command {
		Some(Command::Limit { percent }) => {
			battery.limit(percent)?;
		}
		Some(Command::Clear) => {
			battery.clear()?;
		}
		Some(Command::Persist { percent }) => {
			battery.persist(percent)?;
		}
		Some(Command::Readme) => {
			// Compile-time include from the repo root
			print!("{}", include_str!("../README.md"));
		}
		Some(Command::Unpersist) => {
			battery.unpersist()?;
		}
		Some(Command::Shell { shell }) => {
			clap_complete::generate(shell, &mut Cli::command(), env!("CARGO_PKG_NAME"), &mut io::stdout());
		}
		Some(Command::Info) => battery.info(),
		_ => battery.info(),
	};
	Ok(())
}
