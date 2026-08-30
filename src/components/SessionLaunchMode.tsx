import { useEffect, useState } from "react";

import { invoke } from "@tauri-apps/api/core";

import { Switch } from "@/components/ui/switch";
import { Tip } from "@/components/ui/tooltip";
import { emitModsChanged } from "@/lib/modsEvents";
import { cn } from "@/lib/utils";

interface Props {
  gamePath: string;
  gameRunning: boolean;
}

export function SessionLaunchMode({ gamePath, gameRunning }: Props) {
  const [enabled, setEnabled] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<boolean>("get_session_launch_enabled")
      .then((value) => {
        setEnabled(value);
        setReady(true);
      })
      .catch((e) => {
        setError(String(e));
        setReady(true);
      });
  }, []);

  async function change(next: boolean) {
    if (!gamePath || busy || gameRunning) return;
    setBusy(true);
    setError(null);
    try {
      await invoke<string>("set_session_launch_enabled", {
        gameRoot: gamePath,
        enabled: next,
      });
      setEnabled(next);
      emitModsChanged({
        modsFolder: `${gamePath}\\MarvelGame\\Marvel\\Content\\Paks\\~mods`,
        source: "Settings",
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  const tooltip = error
    ? error
    : enabled
      ? "Keep Toolkit-managed mods inactive at rest and deploy them only for Toolkit launches."
      : "Use upstream persistent deployment behavior.";

  return (
    <Tip content={tooltip}>
      <label
        className={cn(
          "flex items-center justify-between gap-2 rounded-sm px-2.5 py-1.5 text-[11px]",
          enabled ? "text-foreground" : "text-muted-foreground",
          (!gamePath || gameRunning || busy) && "opacity-60"
        )}
      >
        <span>Session launch</span>
        <Switch
          checked={enabled}
          onCheckedChange={change}
          disabled={!ready || !gamePath || gameRunning || busy}
          className="scale-90"
        />
      </label>
    </Tip>
  );
}
