---
xolotl: minor
---

Policies choose the accounts each subscription provider may use; a provider without an entry has none. Model rules replace `allow_models` and `deny_models`: one ordered `rules` list of allow and deny globs where the first match decides. Turn single models on or off with the new `models` table; rules win over those switches, and `deny_other_models` sets the rest. Limit requests and tokens per provider and per account. The policy editor lists accounts per provider, edits the rules in order or shows the effective models with overrides, can hide denied models, keeps precedence in the model list, and folds each section.
