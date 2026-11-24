# Debugging notes from a GTA V port

Lessons from bringing GTA V (RAGE, D3D11 path) up natively on an Android
phone (ARM64, D3D11 over DXVK and Vulkan on an Adreno 740). That port went
through the stage VIctory is at now: the title loads the world and then
does not move on into the game. What got it past that is written down here
as things to try. None of it has been checked against GTA VI.

Nothing from either game is in this note: no code, names from it or data.

## The world loads, but the game does not start

In the port, the title got through boot, menus and the loading screen,
loaded the map, and then sat there: the loading screen (or a dark frame)
stayed up, memory stopped growing, nothing crashed. It was never one cause.
Each fix moved it a little further, to the next wait. The order below is
the order they turned up in.

### First, find out what it is waiting for

A hang with no crash only shows itself through state. These dumps, taken
while it sat there, are what named each cause:

* **Every script thread with its state and program counter.** Taken
  every few seconds during loading. A script that never leaves its first
  instructions, or one that has faulted, stops the game flow even though
  the engine keeps rendering.
* **The streaming queue.** When the head request stops moving: its file,
  its state, whether the read completed, whether it was placed in memory,
  and how many requests are pending against the loader's limit (30 in that
  engine; past it, nothing new is issued).
* **All threads' stacks** (`VICTORY_STACKS`), to see who waits on what: a
  sleeping thread waiting for a service that never answers looks idle, not
  stuck.
* **Any warning or prompt the title opens**, with its text, logged from the
  title's own call. See the next section on why.

### The title is waiting for something the platform should give it

These were the actual causes, each with what it looked like.

**A prompt nobody can see or answer.** Twice the game was not stuck but
waiting for input. Once a "sign in" warning came up during autoload, and
the screen showed no text because fonts were not drawing yet. Once it was
the first-boot brightness calibration, which waits for a button press, and
input was not wired up. With a dark scene, the title may be showing a
dialog. Logging every warning or prompt with its text (and the button it
waits for) answers that in one run.

**The signed-in user.** The save system asked whether a user was signed
in, and the replacement for the online platform answered no. Autoload then
stopped at the sign-in warning above. Answering "signed in" as a local
offline user fixed it; with no saves, autoload then starts a new game. In
VIctory that is the runtime-services layer: every user, licence and
package query the title makes while loading should get a definite "yes,
offline" answer.

**A background service that never answers.** One thread slept forever
waiting for the online platform's entitlement check. That one was harmless,
but it is the same shape as a real stall: a title thread blocked on a
platform callback, event or async result that is never signalled. Check
that every async operation the HLE layer accepts completes, including the
ones that fail.

**Data it waits on without reporting a missing file.** The loading screen
waited for sound banks that were not there yet: no error, just a wait.
After the banks were added, it moved on.

**Data in a form it did not expect.** Script files were still encrypted
(they used the container's key, unlike the other resources). The script
system loaded them and failed silently. Every converted file should be
verified by a read in the title's own format, not by size.

**The startup script faulting.** The first script faulted with "call
through a null function pointer" in its first instructions, and the game
never left the loading flow. The script VM was not translating the older
bytecode it was given. Natives were not the problem, although they looked
like it. If the title runs scripts, a VM fault log (script, program counter,
reason) is worth having before anything else.

**Budgets and pools from the platform config.** The world's streaming
stalled because a memory budget was 0 MB: it was inherited from another
platform's config and cut down by a heap size that did not match. Later,
crossing from the first area to the main city aborted because a pool of
loaded objects was sized for a smaller data set. In VIctory the title reads
its config for the Xbox. If a memory figure the title asks the platform for
comes back smaller or 0, the title sizes its budgets from it and stalls
when they fill up.

**A platform function returning garbage.** A "how many worker threads"
query had no branch for the new platform and returned garbage. It was used
as an index, and the physics crashed on the first frame of gameplay. In
VIctory the equivalents are system queries with an out of range answer
(processor count, core masks, memory sizes, GPU properties) that the title
uses to size arrays.

**The render thread deadlocking with the main thread.** One fix took the
graphics context for a whole list of commands. The main thread then waited
for the context while the render thread held it waiting for the next list,
and the picture froze. Look at both threads' stacks when the frame freezes
and the process does not crash.

**Streaming reads that never complete.** Sometimes one read request never
finished. The streamer was idle, the loader's queue filled up behind it,
and loading stopped. It was intermittent. In VIctory this is file I/O and
async completion in the HLE layer: a read, or a completion port or event,
that never gets signalled. It looks exactly like this.

### Slow frames make "running" look like "stuck"

Game flow in these engines advances per frame. At 0.5 to 1.8 s per frame, a
cutscene or scripted transition that takes 300 frames needs several minutes.
Before treating the game as stuck, check the script program counters and
the streaming queue over time: if they move, it is running, just slowly.

## A dark scene after loading

**Find the pass where the image goes empty.** In the port the world was
black and characters were flat grey shapes. A dump of every bound render
target before each change of targets (141 passes in one frame) showed that
the G-buffer and lighting were correct. The frame went black in the copy
that feeds post-processing: the title took a branch that skips that copy
on some platforms. More generally, anything the console does implicitly
(render-target metadata, fast clears, resolves) has to be done by the host
layer, or a later pass reads an empty target.

**Auto-exposure turns one bad value into a dark frame.** A luminance
reduction that yields 0 or NaN darkens the whole scene. A missing compute
dispatch in that chain would look the same. Forcing a fixed exposure tells
the two apart.

**A constant that arrives as 0 makes a pass sample texel (0,0).** That gives
one flat colour, exactly that pixel's. Check the constant buffer for zeros.

## GPU hangs

* A half-drawn frame left on screen by a GPU hang looked like a shading bug.
  Separate the two first (WARP or full synchronisation).
* Waiting for the GPU before each present stopped the hangs. So did one
  submission per draw. Both showed the fault depended on CPU/GPU overlap.
* Early fences (the title reusing memory because a fence said the GPU was
  done) were tested and ruled out. In VIctory, title fences map onto host
  fences, so the same check is worth one run.
* The cause there was the phone vendor's driver; switching to Mesa's Turnip
  fixed it.

## What does not carry over

That port recompiled the title for another CPU; VIctory runs its x86-64 code
unchanged. These problems from the port do not apply:

* memory ordering on ARM (empty barriers, counters seen before data);
* compiler differences (signed `char`, null checks after `new`, references
  assumed non-null);
* the data formats and encryption of GTA V's PC release.
