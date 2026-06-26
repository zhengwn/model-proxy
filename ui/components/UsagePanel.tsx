import { useEffect, useMemo, useRef, useState } from "react";
import { Card, Spin, Alert, Button, Typography, Tooltip, Segmented, theme } from "antd";
import { ReloadOutlined } from "@ant-design/icons";
import { invoke } from "@tauri-apps/api/core";
import type { DailyUsage } from "../types";
import { useLocale } from "../i18n";

const { Text } = Typography;

// ~6 months of weekly columns (Mon..Sun rows), GitHub-contribution style.
const WEEKS = 27;
const GAP = 4;
const MIN_CELL = 11;
const MAX_CELL = 26;
const LEGEND_CELL = 13;

// Active intensity ramp — blue, matching the reference. Empty cells use a
// theme-aware gray.
const LEVEL_COLORS = ["#cfe0fb", "#94bdf6", "#4d8af0", "#1f6feb"];

// Stat accent colors in the same blue family.
const STAT_COLORS = ["#1f6feb", "#4d8af0", "#6366f1"];

const EN_MONTHS = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun",
  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

type Mode = "daily" | "weekly" | "cumulative";

function ymd(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function mondayOf(d: Date): Date {
  const r = new Date(d);
  const weekday = (r.getDay() + 6) % 7; // 0 = Mon … 6 = Sun
  r.setDate(r.getDate() - weekday);
  r.setHours(0, 0, 0, 0);
  return r;
}

function trimNum(x: number): string {
  return x.toFixed(2).replace(/\.?0+$/, "");
}

function formatTokens(n: number, locale: string): string {
  if (locale === "zh") {
    if (n >= 1e8) return `${trimNum(n / 1e8)}亿`;
    if (n >= 1e4) return `${trimNum(n / 1e4)}万`;
    return String(n);
  }
  if (n >= 1e9) return `${trimNum(n / 1e9)}B`;
  if (n >= 1e6) return `${trimNum(n / 1e6)}M`;
  if (n >= 1e3) return `${trimNum(n / 1e3)}K`;
  return String(n);
}

function formatDate(ds: string, locale: string): string {
  const [yy, mm, dd] = ds.split("-");
  const y = Number(yy);
  const m = Number(mm);
  const d = Number(dd);
  return locale === "zh" ? `${y}年${m}月${d}日` : `${EN_MONTHS[m - 1]} ${d}, ${y}`;
}

function levelOf(value: number, max: number): number {
  if (value <= 0) return 0;
  return Math.min(4, Math.max(1, Math.ceil((value / max) * 4)));
}

interface Cell {
  date: string;
  level: number; // 0 = empty/unfilled, 1..4 = intensity
  future: boolean;
  tip: string; // precomputed tooltip for the active mode
}

export function UsagePanel() {
  const { t, locale } = useLocale();
  const { token } = theme.useToken();
  const [data, setData] = useState<DailyUsage[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [mode, setMode] = useState<Mode>("daily");

  // Measure available width so the grid stretches to fill the row.
  const wrapRef = useRef<HTMLDivElement>(null);
  const [wrapW, setWrapW] = useState(0);
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      for (const e of entries) setWrapW(e.contentRect.width);
    });
    ro.observe(el);
    setWrapW(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<DailyUsage[]>("get_daily_usage");
      setData(result);
    } catch (e) {
      setError(typeof e === "string" ? e : String(e));
      setData([]);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    load();
  }, []);

  const hasData = (data?.length ?? 0) > 0;

  const { columns, monthLabels, stats } = useMemo(() => {
    const source = data ?? [];
    const tokensByDay = new Map<string, number>();
    for (const d of source) tokensByDay.set(d.date, d.count);

    // Daily-based aggregate stats (independent of the selected view mode).
    let total = 0;
    let peakDay = 0;
    let active = 0;
    for (const d of source) {
      total += d.count;
      if (d.count > peakDay) peakDay = d.count;
      if (d.count > 0) active += 1;
    }

    const today = new Date();
    today.setHours(0, 0, 0, 0);
    const todayStr = ymd(today);
    const firstMonday = mondayOf(today);
    firstMonday.setDate(firstMonday.getDate() - (WEEKS - 1) * 7);

    // First pass: build raw day grid + per-week totals.
    const raw: { date: string; tokens: number; future: boolean }[][] = [];
    const weekTotals: number[] = [];
    for (let w = 0; w < WEEKS; w++) {
      const colStart = new Date(firstMonday);
      colStart.setDate(colStart.getDate() + w * 7);
      const week: { date: string; tokens: number; future: boolean }[] = [];
      let weekSum = 0;
      for (let r = 0; r < 7; r++) {
        const day = new Date(colStart);
        day.setDate(day.getDate() + r);
        const ds = ymd(day);
        const tok = tokensByDay.get(ds) ?? 0;
        weekSum += tok;
        week.push({ date: ds, tokens: tok, future: ds > todayStr });
      }
      raw.push(week);
      weekTotals.push(weekSum);
    }

    const fmtTip = (key: "usage.tipDaily" | "usage.tipWeekly" | "usage.tipCumulative", ds: string, val: number) =>
      t(key, { date: formatDate(ds, locale), tokens: formatTokens(val, locale) });

    let cols: Cell[][];
    if (mode === "daily") {
      // Daily heatmap: each cell colored by that day's tokens.
      const maxDay = Math.max(1, peakDay);
      cols = raw.map((week) =>
        week.map((day) => ({
          date: day.date,
          future: day.future,
          level: day.future ? 0 : levelOf(day.tokens, maxDay),
          tip: day.future ? "" : fmtTip("usage.tipDaily", day.date, day.tokens),
        }))
      );
    } else {
      // Weekly / cumulative: each column is a vertical bar growing from the
      // bottom; its height encodes the week's total (or cumulative) tokens.
      const maxWeek = Math.max(1, ...weekTotals);
      const cumWeek: number[] = [];
      let run = 0;
      for (let w = 0; w < WEEKS; w++) {
        run += weekTotals[w];
        cumWeek[w] = run;
      }
      const isWeekly = mode === "weekly";
      cols = raw.map((week, ci) => {
        const value = isWeekly ? weekTotals[ci] : cumWeek[ci];
        const maxRef = isWeekly ? maxWeek : Math.max(1, total);
        const weekFuture = week[0].future; // whole week is future when its Monday is
        const height = value <= 0 ? 0 : Math.min(7, Math.max(1, Math.round((value / maxRef) * 7)));
        const lvl = levelOf(value, maxRef);
        const tip = weekFuture
          ? ""
          : fmtTip(isWeekly ? "usage.tipWeekly" : "usage.tipCumulative", week[6].date, value);
        return week.map((day, r) => ({
          date: day.date,
          future: weekFuture,
          level: !weekFuture && r >= 7 - height ? lvl : 0,
          tip,
        }));
      });
    }

    // Month labels, skipping a partial first month so they never crowd.
    const labels: { col: number; text: string }[] = [];
    let prevMonth = -1;
    for (let w = 0; w < WEEKS; w++) {
      const colStart = new Date(firstMonday);
      colStart.setDate(colStart.getDate() + w * 7);
      const month = colStart.getMonth();
      if (month !== prevMonth) {
        const text = locale === "zh" ? t("usage.monthFmt", { m: month + 1 }) : EN_MONTHS[month];
        const last = labels[labels.length - 1];
        if (last && w - last.col < 3) labels[labels.length - 1] = { col: w, text };
        else labels.push({ col: w, text });
        prevMonth = month;
      }
    }

    return { columns: cols, monthLabels: labels, stats: { total, peakDay, active } };
  }, [data, locale, t, mode]);

  // Theme-adaptive subtle fill for empty days (light gray / translucent in dark).
  const emptyBg = token.colorFillSecondary;
  const cellColor = (cell: Cell): string => {
    if (cell.future) return "transparent";
    // Keep the gray grid visible in every mode (so it never disappears when
    // usage is zero). In bar views, blue cells form the bar above the gray rest.
    if (cell.level === 0) return emptyBg;
    return LEVEL_COLORS[cell.level - 1];
  };

  const baseW = wrapW > 0 ? wrapW : 640;
  const cellSize = Math.max(
    MIN_CELL,
    Math.min(MAX_CELL, Math.floor((baseW - (WEEKS - 1) * GAP) / WEEKS))
  );
  const colStride = cellSize + GAP;
  const gridWidth = WEEKS * colStride - GAP;

  const statItems = [
    { label: t("usage.totalCalls"), value: formatTokens(stats.total, locale), color: STAT_COLORS[0] },
    { label: t("usage.peakDay"), value: formatTokens(stats.peakDay, locale), color: STAT_COLORS[1] },
    {
      label: t("usage.activeDays"),
      value: `${stats.active}`,
      suffix: t("usage.activeDaysUnit"),
      color: STAT_COLORS[2],
    },
  ];

  const statRow = (
    <div style={{ display: "flex", gap: 12, flexWrap: "wrap" }}>
      {statItems.map((s) => (
        <Card
          key={s.label}
          size="small"
          style={{ flex: "1 1 0", minWidth: 150 }}
          styles={{ body: { padding: "14px 18px" } }}
        >
          <div style={{ display: "flex", alignItems: "baseline", gap: 6 }}>
            <span style={{ fontSize: 26, fontWeight: 600, color: s.color, lineHeight: 1.1 }}>
              {s.value}
            </span>
            {s.suffix && (
              <span style={{ fontSize: 13, color: token.colorTextSecondary }}>{s.suffix}</span>
            )}
          </div>
          <Text type="secondary" style={{ fontSize: 13 }}>
            {s.label}
          </Text>
        </Card>
      ))}
    </div>
  );

  return (
    <Card
      title={t("usage.heatmapTitle")}
      extra={
        <Button icon={<ReloadOutlined />} onClick={load} loading={loading}>
          {t("common.refresh")}
        </Button>
      }
    >
      {loading && !data ? (
        <Spin style={{ display: "block", margin: "48px auto" }} />
      ) : (
        <>
          {error && (
            <Alert type="error" showIcon title={t("usage.loadFailed")} description={error} style={{ marginBottom: 16 }} />
          )}
          {!hasData && !error && (
            <Alert type="info" showIcon title={t("usage.empty")} style={{ marginBottom: 16 }} />
          )}
          <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
            {/* View toggle */}
            <Segmented
              value={mode}
              onChange={(v) => setMode(v as Mode)}
              options={[
                { label: t("usage.daily"), value: "daily" },
                { label: t("usage.weekly"), value: "weekly" },
                { label: t("usage.cumulative"), value: "cumulative" },
              ]}
              style={{ alignSelf: "flex-start" }}
            />
            {/* Heatmap */}
            <div ref={wrapRef} style={{ overflowX: "auto" }}>
              <div style={{ display: "inline-block", minWidth: "min-content" }}>
                {/* Month labels */}
                <div
                  style={{
                    height: 18,
                    position: "relative",
                    width: gridWidth,
                    overflow: "hidden",
                  }}
                >
                  {monthLabels.map((m) => (
                    <span
                      key={`${m.col}-${m.text}`}
                      style={{
                        position: "absolute",
                        left: m.col * colStride,
                        fontSize: 12,
                        color: token.colorTextSecondary,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {m.text}
                    </span>
                  ))}
                </div>
                {/* Grid — keyed by mode so cells replay the pop-in wave on switch */}
                <div style={{ display: "flex" }}>
                  <div key={mode} style={{ display: "flex", gap: GAP }}>
                    {columns.map((week, ci) => (
                      <div key={ci} style={{ display: "flex", flexDirection: "column", gap: GAP }}>
                        {week.map((cell) =>
                          cell.future ? (
                            <div key={cell.date} style={{ width: cellSize, height: cellSize }} />
                          ) : (
                            <Tooltip key={cell.date} title={cell.tip}>
                              <div
                                className="usage-cell"
                                style={{
                                  width: cellSize,
                                  height: cellSize,
                                  borderRadius: 3,
                                  background: cellColor(cell),
                                  animationDelay: `${ci * 11}ms`,
                                }}
                              />
                            </Tooltip>
                          )
                        )}
                      </div>
                    ))}
                  </div>
                </div>
                {/* Legend */}
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "flex-end",
                    gap: 6,
                    marginTop: 12,
                  }}
                >
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {t("usage.less")}
                  </Text>
                  <div style={{ width: LEGEND_CELL, height: LEGEND_CELL, borderRadius: 3, background: emptyBg }} />
                  {LEVEL_COLORS.map((c) => (
                    <div key={c} style={{ width: LEGEND_CELL, height: LEGEND_CELL, borderRadius: 3, background: c }} />
                  ))}
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {t("usage.more")}
                  </Text>
                </div>
              </div>
            </div>
            {/* Stats */}
            {statRow}
          </div>
        </>
      )}
    </Card>
  );
}

export default UsagePanel;
