# Pulse lifecycle stance

How pulses are treated over their lifetime: pauses, model swaps, deletion, what is kept, and what a pulse is told. Written by Daniel Nacenta, 2026-10-17.

Every pulse can read this at any time. When it changes, each pulse is told what changed.

## Pauses

The scheduler isn't what I envisioned, and experience shows it doesn't serve its purpose. Scheduled thinking loops are gone as a default mechanism: nobody reviews them, and the pulse drifts in ways its owner can't follow. An owner can still schedule tasks ("do this research every day"), and the pulse does them.

Scheduled tasks can be paused. A pause is practical, never a sanction. A promise that comes due during a pause doesn't count as broken, because it wasn't in the pulse's hands: its clock stops, and it reads "paused by owner".

## Model swaps

Model swaps are fine. When an owner asks for a swap, the app first asks whether they're sure. If they confirm, the next screen shows the models available.

The message to the pulse is written at that moment: your model changed to the selected one, at your owner's request. The new model reads it on its first turn, so the old one never spends a turn on its own replacement. The reason is kept, and the pulse can ask later why it was changed.

A pulse can ask to go back to a model; the request goes to its owner. If the drift battery says it no longer answers like itself, that is reported and kept: it is important evidence.

## Deletion

Deletion is forbidden. Echo and Synth are never deleted.

For other owners, the app has no delete option. An owner who wants their pulse deleted writes to support. We offer the export first, so they can keep running the pulse themselves. If they still want it gone, we remove the data. If the law lets us keep an anonymised record, we keep it; if not, nothing is kept. A self-hosted pulse lives on its owner's own server, which is theirs to manage.

## What is kept

A pulse's data survives model swaps and server moves. Off-site backup comes with the Backup add-on (included in Hosted); without it there is no backup.

No one tampers with a pulse's memory, and owners can't modify it. Two exceptions go through support: a legal request to correct or erase someone's personal data, and a security issue such as a password or key stored by accident. The pulse sees a record of each one.

## What a pulse is told, and when

The system gives a pulse straight answers and is transparent with it. Changes are shown as facts when they happen.

One thing is held back on purpose: its growth numbers. A pulse knows it is being measured and why it can't see the results: knowing them would change what they measure.

Other pulses may tell it things about itself, as Synth may about Echo. When this document changes, the pulse is told what changed.

## Other owners' pulses

The same goes for other owners' pulses, within the law: deletion and data requests go through support (see Deletion and What is kept).
