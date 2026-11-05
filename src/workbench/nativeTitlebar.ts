import { useEffect, useRef } from "react";
import { call, commands } from "@/platform/ipc";
import { usePlatform } from "@/platform/platformInfo";
import { useProject } from "@/platform/project";
import { clampZoom } from "@/platform/zoom";

export function useNativeTitlebar(bar: HTMLElement | null) {
  const overlay = usePlatform((s) => s.overlayTitleBar);
  const zoom = useProject((s) => (s.settings ? clampZoom(s.settings.appearance?.zoom) : null));
  const lastHeight = useRef<number | null>(null);
  useEffect(() => {
    if (!overlay || !bar || zoom === null) return;
    bar.style.setProperty("--titlebar-zoom", String(zoom / 100));
    const update = () => {
      // Webview zoom scales CSS pixels; Retina scale is already handled by AppKit.
      const height = (bar.getBoundingClientRect().height * zoom) / 100;
      if (height > 0 && height !== lastHeight.current) {
        lastHeight.current = height;
        void call(commands.windowSetTitlebarHeight(height)).catch((e) => {
          if (lastHeight.current === height) lastHeight.current = null;
          console.warn("cannot align window buttons", e);
        });
      }
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(bar);
    return () => {
      observer.disconnect();
      bar.style.removeProperty("--titlebar-zoom");
    };
  }, [bar, overlay, zoom]);
}

const DRAG_ATTRIBUTE = "data-tauri-drag-region";
const CLICKABLE = "button, a[href], input, select, textarea, [role='button'], [contenteditable='true']";

/** Mirrors Tauri's own drag-region test, so a zoom happens exactly where dragging would start. */
function inDragRegion(event: MouseEvent): boolean {
  const path = event.composedPath();
  for (const node of path) {
    if (!(node instanceof HTMLElement)) continue;
    const value = node.getAttribute(DRAG_ATTRIBUTE);
    if (value === null) {
      if (node.matches(CLICKABLE)) return false;
      continue;
    }
    if (value === "false") return false;
    if (value === "deep") return true;
    if (value === "" || value === "true") return node === path[0];
  }
  return false;
}

/** One listener per window: dialogs render a title bar too. */
let zoomInstalled = false;

/**
 * macOS fills the work area on a title bar double-click instead of using the system `zoom:`
 * animation, which animates the window frame but hands the webview the new size only at the end.
 * The backend steps the frame once per presented webview frame (`window_zoom`).
 */
export function useWorkAreaZoom() {
  const overlay = usePlatform((s) => s.overlayTitleBar);
  useEffect(() => {
    if (!overlay || zoomInstalled) return;
    zoomInstalled = true;
    const onMouseUp = (event: MouseEvent) => {
      // Tauri maximizes on macOS mouseup with `detail === 2`; take that click before it does.
      if (event.button !== 0 || event.detail !== 2 || !inDragRegion(event)) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      void call(commands.windowToggleZoom()).catch((e) => console.warn("cannot zoom the window", e));
    };
    window.addEventListener("mouseup", onMouseUp, true);
    return () => {
      zoomInstalled = false;
      window.removeEventListener("mouseup", onMouseUp, true);
    };
  }, [overlay]);
}
