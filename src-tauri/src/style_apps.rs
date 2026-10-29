//! Discover launchable apps for style rules; foreground detection remains separate.
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct StyleApp {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub suggested_category: Option<crate::styles::Category>,
}

fn suggest(metadata: &str) -> Option<crate::styles::Category> {
    let text = metadata.to_ascii_lowercase();
    if text.contains("email") || text.contains("mail") {
        Some(crate::styles::Category::Email)
    } else if ["office", "development", "terminal", "ide"]
        .iter()
        .any(|s| text.contains(s))
    {
        Some(crate::styles::Category::Work)
    } else if ["chat", "instantmessaging", "social"]
        .iter()
        .any(|s| text.contains(s))
    {
        Some(crate::styles::Category::Personal)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn discover() -> Vec<StyleApp> {
    use std::path::PathBuf;
    let mut dirs = Vec::new();
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    let system =
        std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&system));
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            if !seen.insert(id.to_ascii_lowercase()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut section = false;
            let (mut name, mut class, mut exec, mut categories, mut hidden, mut kind) =
                (None, None, None, "", false, "");
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    section = line == "[Desktop Entry]";
                    continue;
                }
                if !section {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    match key {
                        "Name" => name = Some(value),
                        "StartupWMClass" => class = Some(value),
                        "Exec" => exec = Some(value),
                        "Categories" => categories = value,
                        "Hidden" | "NoDisplay" if value == "true" => hidden = true,
                        "Type" => kind = value,
                        _ => {}
                    }
                }
            }
            if hidden || kind != "Application" {
                continue;
            }
            let Some(name) = name.filter(|n| !n.is_empty()) else {
                continue;
            };
            let mut aliases = vec![id.clone()];
            if let Some(c) = class.filter(|c| !c.is_empty()) {
                aliases.push(c.to_string());
            }
            if let Some(exe) = exec
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.rsplit('/').next())
            {
                if !exe.is_empty() && !["env", "flatpak", "snap"].contains(&exe) {
                    aliases.push(exe.to_string());
                }
            }
            result.push(StyleApp {
                id,
                name: name.to_string(),
                aliases,
                suggested_category: suggest(categories),
            });
        }
    }
    result
}

#[cfg(target_os = "macos")]
fn discover() -> Vec<StyleApp> {
    use std::path::PathBuf;
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("app") {
                continue;
            }
            let output = std::process::Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", "Print :CFBundleIdentifier"])
                .arg(path.join("Contents/Info.plist"))
                .output();
            let Ok(output) = output else { continue };
            if !output.status.success() {
                continue;
            }
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if id.is_empty() || !seen.insert(id.to_lowercase()) {
                continue;
            }
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            result.push(StyleApp {
                aliases: vec![id.clone()],
                suggested_category: suggest(&name),
                id,
                name,
            });
        }
    }
    result
}

#[cfg(target_os = "windows")]
fn discover() -> Vec<StyleApp> {
    // Start Menu shortcuts provide executable paths matching foreground detection.
    let script = r#"$w=New-Object -ComObject WScript.Shell; $dirs=@([Environment]::GetFolderPath('Programs'),[Environment]::GetFolderPath('CommonPrograms')); @(foreach($d in $dirs){Get-ChildItem -LiteralPath $d -Filter '*.lnk' -Recurse -ErrorAction SilentlyContinue | ForEach-Object {$t=$w.CreateShortcut($_.FullName).TargetPath; if($t -and $t.EndsWith('.exe',[StringComparison]::OrdinalIgnoreCase)){[PSCustomObject]@{name=$_.BaseName; id=[IO.Path]::GetFileName($t).ToLowerInvariant()}}}}) | ConvertTo-Json -Compress"#;
    let Ok(output) = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let entries = match value {
        serde_json::Value::Array(v) => v,
        value => vec![value],
    };
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.to_string();
            let name = v.get("name")?.as_str()?.to_string();
            if !seen.insert(id.clone()) {
                return None;
            }
            Some(StyleApp {
                aliases: vec![id.clone()],
                suggested_category: suggest(&name),
                id,
                name,
            })
        })
        .collect()
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn discover() -> Vec<StyleApp> {
    Vec::new()
}

#[tauri::command]
#[specta::specta]
pub async fn list_style_apps() -> Result<Vec<StyleApp>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut apps = discover();
        apps.sort_by_key(|app| app.name.to_lowercase());
        apps
    })
    .await
    .map_err(|e| e.to_string())
}
