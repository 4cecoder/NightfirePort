# WORKLOG

## retune-nb-16 (2026-11-18, s/retune-nb-16)

Task: commit 3412a89a3 made every guard run the game spacing scan and the close-range alt attack from the fire states; the level 16 walks `walk_the_chasers_defeat_completes_the_mission_on_real_damage_agent` and `_00agent` now die. Read both ignores and the 16 walk changes in that commit, fix the fight that kills Bond (real input only, no top-ups, never lower damage or health, never change guard code), helpers stay gated to level+difficulty. Remove each ignore when green; if still red, push the measured ledger and the next lever into the ignore message. Run the whole walkreal_07000016_live binary.

### retune-nb-16 progress

retune-nb-16 progress (2026-11-18): the Agent walk (d2) is GREEN — un-ignored. The lever is one
gate change, file-local: the squad-phase close stands (the 00 Agent law, 120-tick bounded stands on
any live hostile inside 10 u between the member slices) extended to `difficulty >= 2 && !w.tops_up()`.
The measured death it fixes (t25168, reproduced byte-identical first): with the spacing scan live the
post-chaser pit-shooter clear picks NOTHING (no guard exposed from the fight's end spot), so the
pit-room AR guard #15 (0x50000b2, 0x14, hp 20) enters the squad phase alive, hunts Bond through the
10800-tick far-member stand (invisible #13), closes to 2.0 u (inside the 3.0 always-hit band
[M 0x179740:113-115]) and pours 48.7 in 10 hits; the dealer answer fired once (4 of 20) before Bond
died INSIDE it. Green run: squad audit clean (21 hits, every delta within the game's chain), ch
0xbf/0xc0/0x82, the button, MissionState 6. Sisters/punch/d1 untouched (gates verified).

The 00 Agent walk (d3) is still RED at the furthest measured point; the full ledger + the next lever
are in the test's ignore message. Landed d3 levers (all `difficulty >= 3 && !tops_up()`, file-local):
the east-high ring guards join the peek cycles' fight list; the close pair fights from the peek; the
exit stays fought. Measured-worse levers tried and REVERTED (each on its own real run): level_fights
at d3 (t3912 ramp death), the after-exit resupply (t3608), the ramp-foot perch stand (t4230, the gun
is at clip 1 spare 6), east-high x>8.5 + peek r 0.9 (t6008 fist convergence). The ammo chain (the
exit-fight drops 2-6 u off the path; the resupply trail corridor misses them) is the next lever.

Binaries at the final tree: walkreal_07000016_live 18 passed / 1 ignored (the d3 walk, ledgered);
the three smaller crates' full binary sets run in the background at push time (the nf-view binary
set ENOSPCs this sandbox — the report documents it).

### retune-nb-16-4

retune-nb-16-4 (2026-11-19): started. Round 4 of the 00 Agent real-damage walk
(walk_the_chasers_defeat_completes_the_mission_on_real_damage_00agent). Branch from origin/master
2be83dc10; no origin/s/retune-nb-16-2 or -3 branch or report exists anywhere, so master's walk file
and its ignore message are the current ledger truth. Reading the ledger, then the next untried lever.

### retune-nb-16-4 progress

retune-nb-16-4 progress (2026-11-19): the lever-(a) wire trace ran and KILLED the wire theory —
the loot window measures ZERO live-claimed spots on this tree (the only stage flip is the
LiftCrush hurt #1, gate 0xcb, at +3185, its poly the bay lift 12+ u east; the ShuttleLaunch
ch-0xf3 pulses arm only the launch script's own window). The real blocker was the round-2
truncate(2): two kind-0x1 ammo boxes (the Ammo9 w 0x15 the ledger called a med-kit — Pickup
Handler's kind-1 arm is Player_EquipAmmo [M 0x18e650:210-214], 0x15 unowned, it refuses forever;
the "+10.0 health buffer" never existed) truncated the armour (kind 0x3) out of every run, and
Bond died at 12882 to the chaser's three landed rounds with armour 0.0. The fix (d3-!tops_up
gated, file-local): the loot pass moved ahead of the room-soldier fight, sort kit -> armour ->
ammo, truncate(3). Measured: the loot window 156 ticks (armour +90, the Commando +50 spare 64),
the soldier fight costs 0 health, the kill loop takes her at +3508 with health 19.5 untouched,
pulse 2 sat out in the shelter, the squad stands entered at 19.5. NEW death t17282: ONE -19.50
round from squad clone #12 at d 1.4 with armour 0.0 (the 50.0 pool spent across the soldier
fight, the 3508-tick punch-band kill and the squad's early fire; the pit room has NO health
pickup [D the audit's 17-pickup table]). The whole walkreal_07000016_live binary: 18 passed /
0 failed / 1 ignored (the d3 walk, ledgered at t17282 with the next levers in the ignore
message: the east armour #649 fetch, the shorter kill, the in-room re-arm hold).
