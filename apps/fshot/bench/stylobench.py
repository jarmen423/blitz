#!/usr/bin/env python3
"""Wave 1a's measured restyle benchmark (docs/WAVE1A_STYLOBENCH.md in known-future has the numbers and what they mean).

    python3 stylobench.py --fshot FSHOT --scenes DIR --out OUT.json [--scene NAME ...] [--states 100] [--runs 15]

What it times, on prepared scene pages (DIR/<scene>/pages/{static,neutral,base}.html, as `known_future.prepare` writes
them), all on the CPU, with wall clocks:

  cold   `fshot dump page out.json`, a fresh process each time: what the compiler paid for every question
  warm   one `fshot serve`: the process's start, `open` (parse, fetch, style, layout), `dump`, `boxes`, `analyze`
  state  `resolve`: per state the time to apply it, work the styles and the layout out again (the answer's
         `ms.resolve`, taken by the server around exactly that), and the time to collect the report (`ms.report`)

The states: a custom property on the root that nothing uses, that a tenth of the elements use for a paint-only
property, for a layout property, that every element uses; the same on one element of the scene; inline `transform`,
`opacity`, `color`, `clip-path` and `width` on one element. Each kind is a batch of --states states with different
values (so nothing is answered from a cache), asked with the default report (`"nodes": "changed"`, which reads every
element after every state) and with the report of the changed element alone.

The prepared pages of the project measured use no custom property, so the properties that depend on one are put
there by the benchmark, in the page's <style id="fshot-pic"> (rules of the form `[data-i$="7"]{opacity:
var(--kf-a)}`, written into a copy of the page: no state sets them); the DOM, the style sheets and the layout are
the scene's own."""
import argparse, json, os, shutil, statistics, subprocess, sys, tempfile, time

ap = argparse.ArgumentParser()
ap.add_argument("--fshot", required=True)
ap.add_argument("--scenes", required=True, help="a folder of prepared scenes: <scene>/pages/{static,neutral,base}.html")
ap.add_argument("--scene", action="append", default=[], help="a scene to measure (default: the three with most nodes)")
ap.add_argument("--out", required=True)
ap.add_argument("--work", default="", help="a folder for copies of the pages and the dumps (default: a temporary one)")
ap.add_argument("--states", type=int, default=100)
ap.add_argument("--runs", type=int, default=15)
ap.add_argument("--fonts-from", default="", help="replace this prefix of the pages' font addresses ...")
ap.add_argument("--fonts-to", default="", help="... with this one (pages prepared in a tree that is gone)")
a = ap.parse_args()
ENV = {k: v for k, v in os.environ.items() if not k.startswith(("FSHOT_", "BLITZ_"))}
WORK = a.work or tempfile.mkdtemp(prefix="stylobench_")
os.makedirs(WORK, exist_ok=True)


def stats(ms):
    ms = sorted(ms)
    q = lambda f: ms[min(len(ms) - 1, int(round(f * (len(ms) - 1))))]
    return dict(n=len(ms), min=round(ms[0], 3), p50=round(q(0.5), 3), p95=round(q(0.95), 3), max=round(ms[-1], 3), mean=round(statistics.fmean(ms), 3))


class Serve:
    def __init__(self):
        t0 = time.perf_counter()
        self.p = subprocess.Popen([a.fshot, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=ENV, text=True)
        self.ask(cmd="ping")
        self.start_ms = (time.perf_counter() - t0) * 1000

    def ask(self, **kw):
        self.p.stdin.write(json.dumps(kw) + "\n")
        self.p.stdin.flush()
        r = json.loads(self.p.stdout.readline())
        if not r["ok"]:
            raise RuntimeError("%s: %s" % (kw.get("cmd"), r["error"]))
        return r

    def timed(self, **kw):
        t0 = time.perf_counter()
        r = self.ask(**kw)
        return r, (time.perf_counter() - t0) * 1000

    def close(self):
        self.p.stdin.close()
        self.p.wait()


def page(scene, kind):
    text = open(os.path.join(a.scenes, scene, "pages", kind + ".html")).read()
    if a.fonts_from:
        text = text.replace(a.fonts_from, a.fonts_to)
    p = os.path.join(WORK, "%s-%s.html" % (scene, kind))
    with open(p, "w") as fh:
        fh.write(text)
    return p


def load():
    return [float(x) for x in open("/proc/loadavg").read().split()[:3]]


# ---------------------------------------------------------------- which scenes
names = sorted(d for d in os.listdir(a.scenes) if os.path.isfile(os.path.join(a.scenes, d, "pages/base.html")))
s = Serve()
sizes = {}
for name in names:
    st = s.ask(cmd="open", path=page(name, "base"))["stats"]
    sizes[name] = st
s.close()
ranked = sorted(names, key=lambda n: -sizes[n]["nodes"])
scenes = a.scene or ranked[:3]
result = dict(fshot=a.fshot, fshot_sha256=subprocess.run(["sha256sum", a.fshot], capture_output=True, text=True).stdout.split()[0], cpus=os.cpu_count(),
              load_before=load(), states=a.states, runs=a.runs,
              corpus=dict(scenes=len(names), nodes_median=statistics.median(sizes[n]["nodes"] for n in names), nodes_max=max(sizes[n]["nodes"] for n in names),
                          ranked=[(n, sizes[n]["nodes"]) for n in ranked[:8]]),
              scenes={})

for scene in scenes:
    out = result["scenes"][scene] = dict(stats=sizes[scene])
    pages = {k: page(scene, k) for k in ("static", "neutral", "base")}
    out["html_bytes"] = {k: os.path.getsize(p) for k, p in pages.items()}

    # ------------------------------------------------------------ cold
    cold = {}
    for kind, p in pages.items():
        ms = []
        for _ in range(a.runs):
            t0 = time.perf_counter()
            subprocess.run([a.fshot, "dump", p, os.path.join(WORK, "cold.json")], env=ENV, check=True, capture_output=True)
            ms.append((time.perf_counter() - t0) * 1000)
        cold[kind] = stats(ms)
    ms = []
    for _ in range(a.runs):
        t0 = time.perf_counter()
        subprocess.run([a.fshot, "boxes", pages["static"], os.path.join(WORK, "cold.json")], env=ENV, check=True, capture_output=True)
        ms.append((time.perf_counter() - t0) * 1000)
    cold["boxes_static"] = stats(ms)
    out["cold_dump_ms"] = cold

    # ------------------------------------------------------------ warm
    starts = []
    for _ in range(a.runs):
        s = Serve()
        starts.append(s.start_ms)
        s.close()
    s = Serve()
    warm = dict(start_to_first_answer=stats(starts))
    first = s.timed(cmd="open", path=pages["base"])[1]
    warm["first_open_base"] = round(first, 3)  # (reads the system's fonts)
    for kind, p in pages.items():
        ms = [s.timed(cmd="open", path=p)[1] for _ in range(a.runs)]
        warm["open_" + kind] = stats(ms)
        ms = [s.timed(cmd="dump")[1] for _ in range(a.runs)]
        warm["dump_" + kind] = stats(ms)
    s.ask(cmd="open", path=pages["base"])
    warm["open_and_dump_base"] = stats([s.timed(cmd="dump", path=pages["base"])[1] for _ in range(a.runs)])
    warm["boxes"] = stats([s.timed(cmd="boxes")[1] for _ in range(a.runs)])
    warm["analyze_default"] = stats([s.timed(cmd="analyze")[1] for _ in range(a.runs)])
    r = s.ask(cmd="analyze")
    warm["analyze_default_bytes"] = len(json.dumps(r))
    warm["analyze_all"] = stats([s.timed(cmd="analyze", rules=True, paint=True, rects="full", vars="all", sparse=False)[1] for _ in range(a.runs)])
    # the same six questions a scene's compile asks, cold and warm: the two layout pages and four more dumps
    t0 = time.perf_counter()
    for kind in ("static", "neutral", "base", "static", "neutral", "base"):
        subprocess.run([a.fshot, "dump", pages[kind], os.path.join(WORK, "cold.json")], env=ENV, check=True, capture_output=True)
    six_cold = (time.perf_counter() - t0) * 1000
    t0 = time.perf_counter()
    s2 = Serve()
    for kind in ("static", "neutral", "base", "static", "neutral", "base"):
        s2.ask(cmd="dump", path=pages[kind], out=os.path.join(WORK, "warm.json"))
    s2.close()
    warm["six_dumps"] = dict(cold_ms=round(six_cold, 1), warm_ms=round((time.perf_counter() - t0) * 1000, 1))
    out["warm_ms"] = warm

    # ------------------------------------------------------------ states
    an = s.ask(cmd="analyze", props=["display"], used=False, vars="none", text=False, rects=False)
    els = [n for n in an["nodes"] if n.get("kind") == "element" and "i" in n and n.get("rendered", True)]
    kids = {n["n"]: n for n in an["nodes"]}

    def size(n):
        return 1 + sum(size(kids[c]) for c in n.get("children", []) if c in kids and kids[c].get("kind") == "element")
    by_size = sorted(els, key=size)
    leaf = [n for n in by_size if size(n) == 1][len([n for n in by_size if size(n) == 1]) // 2]
    mid = min(by_size, key=lambda n: abs(size(n) - len(els) // 10))   # a subtree of about a tenth of the scene
    big = by_size[-1]
    out["targets"] = {k: dict(i=n["i"], tag=n["tag"], subtree_elements=size(n)) for k, n in (("leaf", leaf), ("subtree", mid), ("largest", big))}
    #: the properties a custom property feeds, put on the scene by the benchmark (module documentation)
    USES = {
        "unused": "/**/",
        "paint_tenth": '[data-i$="7"]{opacity:var(--kf-a,1)}',
        "layout_tenth": '[data-i$="7"]{margin-left:var(--kf-l,0px)}',
        "transform_tenth": '[data-i$="7"]{translate:var(--kf-l,0px) 0px}',
        "paint_all": '[data-i]{opacity:var(--kf-a,1)}',
        "layout_all": '[data-i]{margin-left:var(--kf-l,0px)}',
    }
    N = a.states
    val_a = lambda k: "%.4f" % (0.2 + 0.7 * k / N)
    val_l = lambda k: "%.2fpx" % (0.25 + 3.0 * k / N)

    def use(kind):
        """open the scene with the rules of one kind in its <style id="fshot-pic"> (in the page: no state sets them)"""
        text = open(pages["base"]).read()
        assert '<style id="fshot-pic">/**/</style>' in text
        p = os.path.join(WORK, "%s-base-%s.html" % (scene, kind))
        with open(p, "w") as fh:
            fh.write(text.replace('<style id="fshot-pic">/**/</style>', '<style id="fshot-pic">%s</style>' % USES[kind]))
        s.ask(cmd="open", path=p)

    def states_for(kind, target):
        """N states of one kind; `target` is None for the root"""
        var = {"unused": ("--kf-probe", val_l), "paint_tenth": ("--kf-a", val_a), "paint_all": ("--kf-a", val_a)}.get(kind, ("--kf-l", val_l))
        use(kind)
        if target is None:
            return [{"vars": {var[0]: var[1](k)}} for k in range(N)]
        return [{"set": [{"i": target["i"], "style": {var[0]: var[1](k)}}]} for k in range(N)]

    def inline(prop, target):
        use("unused")
        value = {"transform": lambda k: "translate(%.2fpx, %.2fpx) rotate(%.2fdeg)" % (k * 0.5, k * 0.25, k * 0.1), "opacity": val_a,
                 "color": lambda k: "rgb(%d, %d, 40)" % (k % 256, (k * 7) % 256), "clip-path": lambda k: "inset(%.2fpx)" % (k * 0.05),
                 "width": lambda k: "%.2fpx" % (40 + k * 0.5)}[prop]
        return [{"set": [{"i": target["i"], "style": {prop: value(k)}}]} for k in range(N)]

    def measure(states, nodes=None):
        """one batch: per-state restyle + layout (the server's clock) and report times, and the request's wall clock"""
        kw = dict(states=states, timing=True, vars=[])
        if nodes is not None:
            kw["nodes"] = nodes
        s.ask(cmd="resolve", states=states[:3], vars=[])  # (the first states of a kind pay for what the kind sets up: not measured)
        r, wall = s.timed(cmd="resolve", **kw)
        res, rep = [x["ms"]["resolve"] for x in r["states"]], [x["ms"]["report"] for x in r["states"]]
        changed = [len(x["nodes"]) for x in r["states"]]
        return dict(resolve=stats(res), report=stats(rep), changed_elements_median=statistics.median(changed), batch_wall_ms=round(wall, 1),
                    per_state_wall_ms=round(wall / len(states), 3), baseline_ms=r["ms"]["baseline"], restore_ms=r["ms"]["restore"])

    st = {}
    for kind in USES:
        st["root_var/" + kind] = measure(states_for(kind, None))
    for kind in ("unused", "paint_tenth", "layout_tenth", "paint_all"):
        st["subtree_var/" + kind] = measure(states_for(kind, mid))
        st["leaf_var/" + kind] = measure(states_for(kind, leaf))
    for prop in ("transform", "opacity", "color", "clip-path", "width"):
        st["inline/" + prop + "/leaf"] = measure(inline(prop, leaf))
        st["inline/" + prop + "/subtree"] = measure(inline(prop, mid))
    # the same batches with only the changed element reported (no reading of every element after every state)
    st["root_var/paint_tenth (report: one element)"] = measure(states_for("paint_tenth", None), nodes=[{"i": leaf["i"]}])
    st["inline/transform/leaf (report: one element)"] = measure(inline("transform", leaf), nodes=[{"i": leaf["i"]}])
    st["inline/width/leaf (report: one element)"] = measure(inline("width", leaf), nodes=[{"i": leaf["i"]}])
    # batch size: is a state's cost the same in a batch of 10, 100, 300?
    for n in (10, 50, 300):
        states = [{"vars": {"--kf-a": "%.5f" % (0.2 + 0.7 * k / n)}} for k in range(n)]
        use("paint_tenth")
        r, wall = s.timed(cmd="resolve", states=states, timing=True, vars=[])
        st["root_var/paint_tenth (batch of %d)" % n] = dict(resolve=stats([x["ms"]["resolve"] for x in r["states"]]), report=stats([x["ms"]["report"] for x in r["states"]]),
                                                             batch_wall_ms=round(wall, 1), per_state_wall_ms=round(wall / n, 3), baseline_ms=r["ms"]["baseline"], restore_ms=r["ms"]["restore"])
    out["state_ms"] = st
    # a state by a fresh process, for scale: a page written with the state and dumped cold
    out["cold_state_ms"] = cold["base"]["p50"]
    s.close()

result["load_after"] = load()
with open(a.out, "w") as fh:
    json.dump(result, fh, indent=1)
if not a.work:
    shutil.rmtree(WORK, True)

# ---------------------------------------------------------------- a summary to read
print("fshot %s  sha256 %s  cpus %d  load before %s after %s" % (a.fshot, result["fshot_sha256"][:16], result["cpus"], result["load_before"], result["load_after"]))
for scene, o in result["scenes"].items():
    print("\n## %s: %s" % (scene, o["stats"]))
    print("cold dump p50 ms:", {k: v["p50"] for k, v in o["cold_dump_ms"].items()})
    print("warm p50 ms:", {k: (v["p50"] if isinstance(v, dict) and "p50" in v else v) for k, v in o["warm_ms"].items()})
    print("%-48s %8s %8s %8s | %8s | %8s %s" % ("state", "p50", "p95", "mean", "report", "wall/st", "changed"))
    for k, v in o["state_ms"].items():
        print("%-48s %8.3f %8.3f %8.3f | %8.3f | %8.3f %s" % (k, v["resolve"]["p50"], v["resolve"]["p95"], v["resolve"]["mean"], v["report"]["p50"], v["per_state_wall_ms"],
                                                         v.get("changed_elements_median", "")))
