Move this session onto a different machine type.

Resizing restarts the machine. Every process you have running dies with it — servers, watchers, background builds, anything started in the terminal — and the harness session resumes on the new machine afterwards. The disk survives untouched: the checkout, its build caches and any uncommitted edits are exactly as you left them. Commit or otherwise persist anything you care about that is not already on disk before you call this.

This tool refuses while the working tree has uncommitted changes. If you mean to resize anyway, call it again with `force: true` and say why in `reason`: the changes stay on the disk, but nothing else about the running state does.

You may move to any of these types and only these — the curated catalog for the account and region this session's disk already lives in, which is the same list the user's own machine chooser shows:

{% for line in lines %}
- {% include "machine_line.txt" %}
{%- endfor %}
{% if has_license_bound %}
A type quoting a minimum charge is never resized to on your own authority: flyco raises an approval instead, this tool tells you the request is pending the user's decision, and the machine moves only if the user agrees.
{% endif %}
{%- if user_chose %}
{% include "user_chose_machine.txt" %}
{%- endif %}
