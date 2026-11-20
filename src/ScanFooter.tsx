// The foot of the sidebar: how fresh the index is, the progress of a
// running scan, and its controls (CONCEPT 5.5). The popover holds the
// counters, the nodes being read and the options for the next scan.

import { Show } from "solid-js";
import type { Connection } from "./api";
import type { ScanModel } from "./scanModel";
import Icon from "./ui/Icon";
import Popover from "./ui/Popover";
import Select from "./ui/Select";

export default function ScanFooter(props: { conn: Connection; scan: ScanModel }) {
  const s = props.scan;
  return (
    <div class="sidebar-footer">
      <div class="scan-line">
        <span class="scan-status truncate" classList={{ "is-live": s.live() && !s.active() }} title={s.status()}>
          {s.status()}
        </span>
        <Show when={s.canPause()}>
          <button class="btn-icon" onClick={s.pause} title="Pause scan" aria-label="Pause scan">
            <Icon name="pause" />
          </button>
        </Show>
        <Show when={s.isPaused()}>
          <button class="btn-icon" onClick={s.resume} title="Resume scan" aria-label="Resume scan">
            <Icon name="play" />
          </button>
        </Show>
        <Show when={s.canStop()}>
          <button class="btn-icon" onClick={s.stop} title="Stop this scan and drop the queued ones" aria-label="Stop this scan and drop the queued ones">
            <Icon name="x" />
          </button>
        </Show>
        <Show when={s.canRescan()}>
          <button class="btn-icon" onClick={s.rescan} title={s.rescanLabel()} aria-label={s.rescanLabel()}>
            <Icon name={s.rescanIcon()} />
          </button>
        </Show>
        <Popover
          up
          align="end"
          class="scan-popover"
          trigger={(ctl) => (
            <button class="btn-icon" classList={{ active: ctl.open() }} onClick={ctl.toggle} title="Scan details and options" aria-label="Scan details and options">
              <Icon name="more" />
            </button>
          )}
        >
          {() => (
            <>
              <Show when={s.stats()}>
                <p class="scan-detail">{s.stats()}</p>
              </Show>
              <Show when={s.nodesLine()}>
                <p class="scan-detail dim-label">{s.nodesLine()}</p>
              </Show>
              <Show when={s.notes()}>
                <p class="scan-detail dim-label">{s.notes()}</p>
              </Show>
              <div class="boxed-list">
                <div class="row" title="Everything: one background scan of all key names. On demand: a sample first, then each subtree when it is opened. None: plain SCAN pages, no tree, no counts.">
                  <div class="row-content">
                    <span class="title">Index</span>
                  </div>
                  <div class="suffix">
                    <Select compact value={s.modeValue()} disabled={s.modeBusy()} onChange={(e) => s.setMode(e.currentTarget.value)} aria-label="Index">
                      <option value="">{s.bySizeLabel()}</option>
                      <option value="full">Everything</option>
                      <option value="onDemand">On demand</option>
                      <option value="none">None</option>
                    </Select>
                  </div>
                </div>
                <div class="row" title="Fast: continuous. Normal: small gaps, backs off when the server gets busy. Polite: replicas only, 50 ms gaps.">
                  <div class="row-content">
                    <span class="title">Scan speed</span>
                  </div>
                  <div class="suffix">
                    <Select compact value={props.conn.scanProfile} onChange={(e) => s.setProfile(e.currentTarget.value)} aria-label="Scan speed">
                      <option value="fast">Fast</option>
                      <option value="normal">Normal</option>
                      <option value="polite">Polite</option>
                    </Select>
                  </div>
                </div>
                <div class="row" title="Auto: a replica when one is reachable. Primary: the primary (or primaries). Replica: replicas only.">
                  <div class="row-content">
                    <span class="title">Scan from</span>
                  </div>
                  <div class="suffix">
                    <Select compact value={props.conn.scanTarget} onChange={(e) => s.setTarget(e.currentTarget.value)} aria-label="Scan from">
                      <option value="auto">Auto</option>
                      <option value="primary">Primary</option>
                      <option value="replica">Replica</option>
                    </Select>
                  </div>
                </div>
              </div>
              <p class="caption dim-label">{s.optionsNote()}</p>
            </>
          )}
        </Popover>
      </div>
      <Show when={s.active()}>
        <div class="progress" role="progressbar" aria-valuenow={s.barNow()} aria-valuemin="0" aria-valuemax="100">
          <div class="bar" style={{ width: s.barWidth() }} />
        </div>
      </Show>
    </div>
  );
}
