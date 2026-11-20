import { useCallback, useEffect, useRef, useState } from "react";
import "./Refresh.css";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import {
  LuCircleAlert,
  LuPlus,
  LuRefreshCw,
  LuTrash2,
} from "react-icons/lu";
import { useError } from "../ErrorContext";
import { useDialog } from "../DialogContext";
import { AppError } from "../errors";

type ManagedApp = {
  bundleId: string;
  name: string;
  ipaPath: string;
  addedAt: number;
  lastRefreshedAt: number | null;
  lastAttemptAt: number | null;
  lastError: string | null;
};

type RefreshConfig = {
  autoRefresh: boolean;
  intervalHours: number;
  autoRevokeCerts: boolean;
  apps: ManagedApp[];
};

type RefreshProgress = {
  bundleId: string;
  name: string;
  status: "started" | "success" | "error";
  message: string | null;
};

const formatTime = (timestamp: number) =>
  new Date(timestamp * 1000).toLocaleString();

export const Refresh = ({ ensureReady }: { ensureReady: () => boolean }) => {
  const { t } = useTranslation();
  const { err } = useError();
  const { confirm } = useDialog();
  const [config, setConfig] = useState<RefreshConfig | null>(null);
  const [intervalHours, setIntervalHours] = useState("120");
  const [pending, setPending] = useState<string[]>([]);
  const [running, setRunning] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const intervalFocused = useRef(false);
  const configRef = useRef<RefreshConfig | null>(null);
  const saveSeq = useRef(0);
  const savesInFlight = useRef(0);
  const reloadQueued = useRef(false);

  const applyConfig = useCallback((next: RefreshConfig) => {
    configRef.current = next;
    setConfig(next);
    if (!intervalFocused.current) {
      setIntervalHours(String(next.intervalHours));
    }
  }, []);

  const reload = useCallback(async () => {
    if (savesInFlight.current > 0) {
      reloadQueued.current = true;
      return;
    }
    const next = await invoke<RefreshConfig>("refresh_get_config");
    if (savesInFlight.current > 0) {
      reloadQueued.current = true;
      return;
    }
    applyConfig(next);
  }, [applyConfig]);

  useEffect(() => {
    reload().catch((e) => {
      console.error("Failed to load refresh config", e);
      err(t("refresh.failed_load"), e as AppError);
    });
    invoke<boolean>("autostart_get")
      .then(setAutostart)
      .catch((e) => console.error("Failed to read autostart state", e));
  }, [reload]);

  useEffect(() => {
    const unlisten = listen("refresh-library-changed", () => {
      reload().catch((e) =>
        console.error("Failed to reload refresh config", e),
      );
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [reload]);

  useEffect(() => {
    const unlisten = listen<RefreshProgress>("refresh-progress", (event) => {
      const progress = event.payload;
      setPending((current) =>
        progress.status === "started"
          ? current.includes(progress.bundleId)
            ? current
            : [...current, progress.bundleId]
          : current.filter((id) => id !== progress.bundleId),
      );
      if (progress.status === "success") {
        toast.success(t("refresh.success_toast", { name: progress.name }));
      } else if (progress.status === "error") {
        toast.error(
          t("refresh.error_toast", {
            name: progress.name,
            message: progress.message ?? t("refresh.failed"),
          }),
        );
      }
      reload().catch((e) => console.error("Failed to reload refresh config", e));
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [reload, t]);

  const saveSettings = async (patch: Partial<RefreshConfig>) => {
    const previous = configRef.current;
    if (!previous) return;
    const merged = { ...previous, ...patch };
    if (
      merged.autoRefresh === previous.autoRefresh &&
      merged.intervalHours === previous.intervalHours &&
      merged.autoRevokeCerts === previous.autoRevokeCerts
    ) {
      return;
    }
    configRef.current = merged;
    setConfig(merged);
    const seq = ++saveSeq.current;
    savesInFlight.current += 1;
    try {
      await invoke("refresh_update_settings", {
        settings: {
          autoRefresh: merged.autoRefresh,
          intervalHours: merged.intervalHours,
          autoRevokeCerts: merged.autoRevokeCerts,
        },
      });
    } catch (e) {
      if (seq === saveSeq.current) {
        if ("intervalHours" in patch) {
          setIntervalHours(String(previous.intervalHours));
        }
        reload().catch((reloadError) =>
          console.error("Failed to reload refresh config", reloadError),
        );
      }
      err(t("refresh.failed_save_settings"), e as AppError);
    } finally {
      savesInFlight.current -= 1;
      if (savesInFlight.current === 0 && reloadQueued.current) {
        reloadQueued.current = false;
        reload().catch((e) =>
          console.error("Failed to reload refresh config", e),
        );
      }
    }
  };

  const toggleAutostart = async (enabled: boolean) => {
    setAutostart(enabled);
    try {
      await invoke("autostart_set", { enabled });
    } catch (e) {
      setAutostart(!enabled);
      err(t("refresh.failed_autostart"), e as AppError);
    }
  };

  const addIpa = async () => {
    const path = await openFileDialog({
      multiple: false,
      filters: [{ name: t("app.ipa_files"), extensions: ["ipa"] }],
    });
    if (!path) return;
    try {
      const added = await invoke<ManagedApp>("refresh_add_ipa", { path });
      toast.success(t("refresh.add_success", { name: added.name }));
      await reload();
    } catch (e) {
      err(t("refresh.failed_add"), e as AppError);
    }
  };

  const startRefresh = async (bundleIds: string[]) => {
    if (!ensureReady()) return;
    setRunning(true);
    try {
      await invoke("refresh_now", { bundleIds });
    } catch (e) {
      err(t("refresh.failed_refresh"), e as AppError);
    } finally {
      setRunning(false);
      reload().catch((e) => console.error("Failed to reload refresh config", e));
    }
  };

  const removeApp = (app: ManagedApp) => {
    confirm(
      t("refresh.remove_title"),
      t("refresh.remove_message", { name: app.name }),
      () => {
        invoke("refresh_remove_app", { bundleId: app.bundleId })
          .then(() => reload())
          .catch((e) => err(t("refresh.failed_remove"), e as AppError));
      },
    );
  };

  if (!config) return null;

  const busy = running || pending.length > 0;
  const commitInterval = () => {
    intervalFocused.current = false;
    const value = Math.round(Number(intervalHours));
    if (Number.isFinite(value) && value >= 1 && value <= 720) {
      saveSettings({ intervalHours: value });
    } else {
      setIntervalHours(String(config.intervalHours));
    }
  };

  return (
    <div className="refresh-container">
      <div className="refresh-settings">
        <label className="refresh-toggle">
          {t("refresh.auto")}
          <input
            type="checkbox"
            checked={config.autoRefresh}
            onChange={(e) => saveSettings({ autoRefresh: e.target.checked })}
          />
        </label>
        <label className="refresh-toggle">
          {t("refresh.interval")}
          <input
            className="refresh-interval"
            type="number"
            min={1}
            max={720}
            value={intervalHours}
            onFocus={() => {
              intervalFocused.current = true;
            }}
            onChange={(e) => setIntervalHours(e.target.value)}
            onBlur={commitInterval}
          />
        </label>
        <label className="refresh-toggle">
          {t("refresh.auto_revoke")}
          <input
            type="checkbox"
            checked={config.autoRevokeCerts}
            onChange={(e) =>
              saveSettings({ autoRevokeCerts: e.target.checked })
            }
          />
        </label>
        <label className="refresh-toggle">
          {t("refresh.autostart")}
          <input
            type="checkbox"
            checked={autostart}
            onChange={(e) => toggleAutostart(e.target.checked)}
          />
        </label>
      </div>
      <div className="refresh-actions">
        <button onClick={addIpa}>
          <LuPlus aria-hidden="true" />
          {t("refresh.add_ipa")}
        </button>
        <button
          onClick={() => startRefresh(config.apps.map((app) => app.bundleId))}
          disabled={busy || config.apps.length === 0}
        >
          <LuRefreshCw
            aria-hidden="true"
            className={busy ? "refresh-spin" : undefined}
          />
          {t("refresh.refresh_all")}
        </button>
      </div>
      {config.apps.length > 0 && (
        <div className="refresh-list">
          {config.apps.map((app) => {
            const isPending = pending.includes(app.bundleId);
            return (
              <div className="refresh-item" key={app.bundleId}>
                <div className="refresh-item-text">
                  <span className="refresh-item-name">{app.name}</span>
                  <span
                    className={`refresh-item-meta${isPending ? " refresh-item-pending" : ""}${app.lastError && !isPending ? " refresh-item-error" : ""}`}
                  >
                    {isPending && (
                      <LuRefreshCw
                        className="refresh-spin"
                        aria-hidden="true"
                      />
                    )}
                    {!isPending && app.lastError && (
                      <LuCircleAlert aria-hidden="true" />
                    )}
                    {isPending
                      ? t("refresh.in_progress")
                      : (app.lastError ??
                        (app.lastRefreshedAt
                          ? t("refresh.last_refreshed", {
                              time: formatTime(app.lastRefreshedAt),
                            })
                          : t("refresh.never_refreshed")))}
                  </span>
                </div>
                <div className="refresh-item-actions">
                  <button
                    className="refresh-icon-button"
                    title={t("refresh.refresh_app")}
                    aria-label={t("refresh.refresh_app")}
                    disabled={busy}
                    onClick={() => startRefresh([app.bundleId])}
                  >
                    <LuRefreshCw aria-hidden="true" />
                  </button>
                  <button
                    className="refresh-icon-button danger"
                    title={t("refresh.remove_app")}
                    aria-label={t("refresh.remove_app")}
                    disabled={isPending}
                    onClick={() => removeApp(app)}
                  >
                    <LuTrash2 aria-hidden="true" />
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};
