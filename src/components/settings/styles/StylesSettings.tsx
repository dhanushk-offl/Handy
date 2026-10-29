import React, { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  Dropdown,
  SettingContainer,
  SettingsGroup,
  ToggleSwitch,
} from "@/components/ui";
import { Button } from "@/components/ui/Button";

type Category = "personal" | "work" | "email" | "other";
type WritingStyle = "formal" | "casual" | "very_casual";
type StyleSettings = {
  enabled: boolean;
  model_id: string;
  default_category: Category;
  categories: Record<Category, WritingStyle>;
  apps: Record<string, Category>;
  app_aliases: Record<string, string>;
  custom_prompts: Partial<Record<Category, string>>;
};
type StyleModel = {
  id: string;
  name: string;
  size: number;
  downloaded: boolean;
};
type Progress = { id: string; received: number; total: number };
type InstalledApp = {
  id: string;
  name: string;
  aliases: string[];
  suggested_category: Category | null;
};
const categories: Category[] = ["personal", "work", "email", "other"];
const styles: WritingStyle[] = ["formal", "casual", "very_casual"];

export const StylesSettings: React.FC = () => {
  const { t } = useTranslation();
  const [settings, setSettings] = useState<StyleSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [promptDrafts, setPromptDrafts] = useState<
    Partial<Record<Category, string>>
  >({});
  const [promptCategory, setPromptCategory] = useState<Category>("personal");
  const [models, setModels] = useState<StyleModel[]>([]);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [downloading, setDownloading] = useState<string | null>(null);
  const refreshModels = () =>
    invoke<StyleModel[]>("list_style_models").then(setModels);
  const [capturing, setCapturing] = useState(false);
  const [detected, setDetected] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [installedApps, setInstalledApps] = useState<InstalledApp[]>([]);
  const [appSearch, setAppSearch] = useState("");
  const [selectedApp, setSelectedApp] = useState<string | null>(null);
  const [appsLoading, setAppsLoading] = useState(true);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => {
    void refreshModels().catch(() => toast.error(t("styles.modelError")));
    const unlisten = listen<Progress>("style-model-progress", (event) =>
      setProgress(event.payload),
    );
    invoke<StyleSettings>("get_style_settings")
      .then(setSettings)
      .catch(() => toast.error(t("styles.saveError")));
    invoke<InstalledApp[]>("list_style_apps")
      .then(setInstalledApps)
      .catch(() => toast.error(t("styles.appListError")))
      .finally(() => setAppsLoading(false));
    return () => {
      clearTimeout(timer.current);
      void unlisten.then((fn) => fn());
    };
  }, [t]);
  const save = async (next: StyleSettings) => {
    setSaving(true);
    try {
      await invoke("set_style_settings", { settings: next });
      setSettings(next);
      return true;
    } catch {
      toast.error(t("styles.saveError"));
      return false;
    } finally {
      setSaving(false);
    }
  };
  const categoryOptions = categories.map((value) => ({
    value,
    label: t(`styles.categories.${value}`),
  }));
  const styleOptions = styles.map((value) => ({
    value,
    label: t(`styles.options.${value}`),
    description: t(`styles.optionDescriptions.${value}`),
  }));
  const capture = () => {
    setCapturing(true);
    setUnavailable(false);
    setDetected(null);
    timer.current = setTimeout(async () => {
      try {
        const app = await invoke<string | null>("foreground_app");
        if (app) {
          const alias = Object.entries(settings?.app_aliases ?? {}).find(
            ([key]) => key.toLowerCase() === app.toLowerCase(),
          );
          setDetected(alias?.[1] ?? app);
          setSelectedApp(alias?.[1] ?? app);
          setAppSearch("");
        } else setDetected(null);
        setUnavailable(!app);
      } catch {
        setUnavailable(true);
      } finally {
        setCapturing(false);
      }
    }, 5000);
  };
  if (!settings) return null;
  const savedPrompt = settings.custom_prompts[promptCategory] ?? "";
  const promptValue = promptDrafts[promptCategory] ?? savedPrompt;
  const promptChanged = promptValue !== savedPrompt;
  const knownApps = [...installedApps];
  for (const id of [
    ...Object.keys(settings.apps),
    ...(detected ? [detected] : []),
  ]) {
    if (!knownApps.some((app) => app.id.toLowerCase() === id.toLowerCase())) {
      knownApps.push({ id, name: id, aliases: [id], suggested_category: null });
    }
  }
  const visibleApps = knownApps.filter((app) =>
    `${app.name} ${app.id}`
      .toLowerCase()
      .includes(appSearch.trim().toLowerCase()),
  );
  const selectedAppInfo = knownApps.find((app) => app.id === selectedApp);
  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup
        title={t("styles.title")}
        description={t("styles.description")}
      >
        <ToggleSwitch
          checked={settings.enabled}
          onChange={(enabled) => void save({ ...settings, enabled })}
          disabled={saving}
          label={t("styles.enable")}
          description={t("styles.enableDescription")}
          grouped
        />
        <SettingContainer
          title={t("styles.defaultCategory")}
          description={t("styles.defaultDescription")}
          grouped
        >
          <Dropdown
            options={categoryOptions}
            selectedValue={settings.default_category}
            onSelect={(value) =>
              void save({ ...settings, default_category: value as Category })
            }
            disabled={saving}
          />
        </SettingContainer>
      </SettingsGroup>
      <SettingsGroup
        title={t("styles.categoryTitle")}
        description={t("styles.categoryDescription")}
      >
        {categories.map((category) => (
          <SettingContainer
            key={category}
            title={t(`styles.categories.${category}`)}
            description={t("styles.categoryDescription")}
            grouped
          >
            <Dropdown
              options={styleOptions}
              selectedValue={settings.categories[category]}
              onSelect={(value) =>
                void save({
                  ...settings,
                  categories: {
                    ...settings.categories,
                    [category]: value as WritingStyle,
                  },
                })
              }
              disabled={saving}
            />
          </SettingContainer>
        ))}
      </SettingsGroup>
      <SettingsGroup
        title={t("styles.customPrompts")}
        description={t("styles.customPromptsDescription")}
      >
        <div className="p-4 space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <span className="text-sm font-medium">
              {t("styles.promptCategory")}
            </span>
            <Dropdown
              options={categoryOptions.map((option) => ({
                ...option,
                label: `${option.label}${promptDrafts[option.value] !== undefined && promptDrafts[option.value] !== (settings.custom_prompts[option.value] ?? "") ? " •" : ""}`,
              }))}
              selectedValue={promptCategory}
              onSelect={(value) => setPromptCategory(value as Category)}
              disabled={saving}
            />
          </div>
          <label className="sr-only" htmlFor="style-custom-prompt">
            {t("styles.customPromptLabel", {
              category: t(`styles.categories.${promptCategory}`),
            })}
          </label>
          <textarea
            id="style-custom-prompt"
            className="w-full min-h-28 rounded-lg border border-line bg-background p-3 text-sm focus:outline-none focus:ring-1 focus:ring-logo-primary"
            value={promptValue}
            placeholder={t("styles.customPromptPlaceholder")}
            disabled={saving}
            onChange={(event) =>
              setPromptDrafts((drafts) => ({
                ...drafts,
                [promptCategory]: event.target.value,
              }))
            }
          />
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-xs text-ink-3" aria-live="polite">
              {promptChanged
                ? t("styles.promptUnsaved")
                : savedPrompt.trim()
                  ? t("styles.promptSaved")
                  : t("styles.promptDefault")}
            </p>
            <div className="flex gap-2">
              {promptChanged && (
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={saving}
                  onClick={() =>
                    setPromptDrafts((drafts) => {
                      const next = { ...drafts };
                      delete next[promptCategory];
                      return next;
                    })
                  }
                >
                  {t("styles.discardPrompt")}
                </Button>
              )}
              <Button
                variant="secondary"
                size="sm"
                disabled={saving || !promptChanged}
                onClick={() => {
                  const category = promptCategory;
                  void save({
                    ...settings,
                    custom_prompts: {
                      ...settings.custom_prompts,
                      [category]: promptValue,
                    },
                  }).then((saved) => {
                    if (saved)
                      setPromptDrafts((drafts) => {
                        const next = { ...drafts };
                        delete next[category];
                        return next;
                      });
                  });
                }}
              >
                {t("styles.savePrompt")}
              </Button>
            </div>
          </div>
        </div>
      </SettingsGroup>
      <SettingsGroup
        title={t("styles.appsTitle")}
        description={t("styles.appsDescription")}
      >
        <div className="p-4 space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <label className="text-sm font-medium" htmlFor="style-app-search">
              {t("styles.installedApps")}
            </label>
            <Button
              variant="secondary"
              size="sm"
              disabled={capturing || saving}
              onClick={capture}
            >
              {capturing ? t("styles.capturing") : t("styles.capture")}
            </Button>
          </div>
          {unavailable && (
            <p className="text-sm text-ink-3">{t("styles.unavailable")}</p>
          )}
          <input
            id="style-app-search"
            className="w-full rounded-lg border border-line bg-background p-2 text-sm focus:outline-none focus:ring-1 focus:ring-logo-primary"
            value={appSearch}
            onChange={(event) => setAppSearch(event.target.value)}
            placeholder={t("styles.searchApps")}
          />
          <div
            className="max-h-52 overflow-y-auto rounded-lg border border-line"
            aria-label={t("styles.installedApps")}
          >
            {appsLoading && visibleApps.length === 0 ? (
              <p className="p-3 text-sm text-ink-3">
                {t("styles.appsLoading")}
              </p>
            ) : visibleApps.length === 0 ? (
              <p className="p-3 text-sm text-ink-3">
                {t("styles.noAppsFound")}
              </p>
            ) : (
              visibleApps.map((app) => (
                <button
                  key={app.id}
                  type="button"
                  onClick={() => setSelectedApp(app.id)}
                  aria-pressed={selectedApp === app.id}
                  className={`w-full flex items-center justify-between gap-3 px-3 py-2 text-left text-sm hover:bg-wash focus-visible:outline focus-visible:outline-2 focus-visible:outline-logo-primary ${selectedApp === app.id ? "bg-logo-primary/20" : ""}`}
                >
                  <span className="min-w-0 truncate">{app.name}</span>
                  <span className="shrink-0 text-xs text-ink-3">
                    {settings.apps[app.id]
                      ? t(`styles.categories.${settings.apps[app.id]}`)
                      : t("styles.appDefault")}
                  </span>
                </button>
              ))
            )}
          </div>
          {selectedAppInfo ? (
            <div className="rounded-lg border border-line bg-wash p-3 space-y-3">
              <div>
                <p className="text-sm font-medium break-words">
                  {selectedAppInfo.name}
                </p>
                <p className="text-xs text-ink-3 break-all">
                  {selectedAppInfo.id}
                </p>
              </div>
              {selectedAppInfo.suggested_category &&
                !settings.apps[selectedAppInfo.id] && (
                  <p className="text-xs text-ink-3">
                    {t("styles.suggested", {
                      category: t(
                        `styles.categories.${selectedAppInfo.suggested_category}`,
                      ),
                    })}
                  </p>
                )}
              <div className="flex flex-wrap items-center justify-between gap-3">
                <span className="text-sm">{t("styles.appCategory")}</span>
                <Dropdown
                  options={[
                    {
                      value: "__default",
                      label: t("styles.useDefaultCategory"),
                    },
                    ...categoryOptions,
                  ]}
                  selectedValue={
                    settings.apps[selectedAppInfo.id] ?? "__default"
                  }
                  disabled={saving}
                  onSelect={(value) => {
                    const apps = { ...settings.apps };
                    const aliases = { ...settings.app_aliases };
                    if (value === "__default") {
                      delete apps[selectedAppInfo.id];
                      for (const [alias, id] of Object.entries(aliases))
                        if (id === selectedAppInfo.id) delete aliases[alias];
                    } else {
                      apps[selectedAppInfo.id] = value as Category;
                      for (const alias of selectedAppInfo.aliases) {
                        const existing = Object.entries(aliases).find(
                          ([key]) => key.toLowerCase() === alias.toLowerCase(),
                        );
                        if (!existing || existing[1] === selectedAppInfo.id)
                          aliases[alias] = selectedAppInfo.id;
                      }
                    }
                    void save({ ...settings, apps, app_aliases: aliases });
                  }}
                />
              </div>
            </div>
          ) : (
            <p className="text-xs text-ink-3">{t("styles.selectAppHint")}</p>
          )}
        </div>
      </SettingsGroup>
      <SettingsGroup
        title={t("styles.localModels")}
        description={t("styles.localModelsDescription")}
      >
        {models.map((model) => (
          <div key={model.id} className="flex items-center gap-3 p-4">
            <div className="flex-1 min-w-0">
              <p className="text-sm font-medium">{model.name}</p>
              <p className="text-xs text-ink-3">
                {t(`styles.modelDescriptions.${model.id}`)}
              </p>
              <p className="text-xs text-ink-3">
                {t("styles.modelSize", {
                  size: Math.round(model.size / 1000000),
                })}
              </p>
              {progress?.id === model.id && downloading === model.id && (
                <p className="text-xs text-ink-3">
                  {t("styles.downloadProgress", {
                    percent: Math.round(
                      (progress.received / progress.total) * 100,
                    ),
                  })}
                </p>
              )}
            </div>
            {model.downloaded ? (
              <>
                <Button
                  variant={
                    settings.model_id === model.id ? "primary" : "secondary"
                  }
                  size="sm"
                  disabled={saving}
                  onClick={() => void save({ ...settings, model_id: model.id })}
                >
                  {settings.model_id === model.id
                    ? t("styles.selected")
                    : t("styles.select")}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={saving || !!downloading}
                  onClick={async () => {
                    try {
                      await invoke("delete_style_model", { id: model.id });
                      await refreshModels();
                    } catch {
                      toast.error(t("styles.modelError"));
                    }
                  }}
                >
                  {t("styles.deleteModel")}
                </Button>
              </>
            ) : (
              <Button
                variant="secondary"
                size="sm"
                disabled={!!downloading && downloading !== model.id}
                onClick={async () => {
                  if (downloading === model.id) {
                    await invoke("cancel_style_model_download");
                    return;
                  }
                  setDownloading(model.id);
                  setProgress(null);
                  try {
                    await invoke("download_style_model", { id: model.id });
                    await refreshModels();
                  } catch (error) {
                    if (String(error) !== "cancelled")
                      toast.error(t("styles.modelError"));
                  } finally {
                    setDownloading(null);
                    setProgress(null);
                  }
                }}
              >
                {downloading === model.id
                  ? t("styles.cancelDownload")
                  : t("styles.download")}
              </Button>
            )}
          </div>
        ))}
      </SettingsGroup>
    </div>
  );
};
