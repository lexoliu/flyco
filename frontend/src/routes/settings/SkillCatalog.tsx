/**
 * Settings → Tools → Add a skill from a marketplace (docs/ux.md §10.2).
 *
 * A skill is a directory with a `SKILL.md` in it, published through a
 * plugin marketplace: a GitHub repository flyco reads. `anthropics/skills`
 * is there for everybody and the user may add any other repository.
 *
 * Two pages, one question each: which skill, then which harness gets it.
 * The second question has an answer already chosen — both harnesses — so
 * the page is a confirmation for anyone who does not care which, and a
 * choice for anyone who does.
 *
 * A marketplace flyco has not read yet says so rather than reading as
 * empty: the control plane reads one on a queue, and "still being read"
 * and "offers nothing" are different answers.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { ArrowLeft, Plus, Trash2 } from "lucide-solid";
import SearchField from "../../components/SearchField";
import ProblemNotice from "../../components/ProblemNotice";
import { createQuery } from "../../lib/query";
import { cx } from "../../lib/cx";
import {
  addMarketplace,
  deleteMarketplace,
  installCatalogSkill,
  listCatalogSkills,
  listMarketplaces,
  type CatalogSkill,
  type MarketplaceView,
  type SkillScope,
} from "../../api/client";
import styles from "./Settings.module.css";

/** Where the flow starts from and returns to. */
const TOOLS = "/settings/tools";

const SCOPE_LABEL: Record<SkillScope, string> = {
  claude: "Claude Code",
  codex: "Codex",
};

/** Both harnesses, which is what a skill is installed for unless told otherwise. */
const EVERY_SCOPE: readonly SkillScope[] = ["claude", "codex"];

type Step = { readonly page: "pick" } | { readonly page: "scope"; readonly skill: CatalogSkill };

export default function SkillCatalog() {
  const navigate = useNavigate();
  const [step, setStep] = createSignal<Step>({ page: "pick" });

  return (
    <section class={styles.section}>
      <Show when={step().page === "pick"}>
        <PickPage onChoose={(skill) => setStep({ page: "scope", skill })} />
      </Show>
      <Show when={step().page === "scope" ? step() : null}>
        {(current) => (
          <ScopePage
            skill={(current() as Extract<Step, { page: "scope" }>).skill}
            onBack={() => setStep({ page: "pick" })}
            onInstalled={() => navigate(TOOLS)}
          />
        )}
      </Show>
    </section>
  );
}

/* ── Page 1: which skill ──────────────────────────────────────────────── */

function PickPage(props: { onChoose: (skill: CatalogSkill) => void }) {
  const [catalog, { refetch }] = createQuery(listCatalogSkills);
  const [query, setQuery] = createSignal("");

  const matches = createMemo(() => {
    const wanted = query().trim().toLowerCase();
    const listed = catalog()?.skills ?? [];
    return wanted === ""
      ? listed
      : listed.filter(
          (skill) =>
            skill.name.toLowerCase().includes(wanted) ||
            skill.description.toLowerCase().includes(wanted),
        );
  });

  /** The marketplaces with at least one match, in listing order. */
  const groups = createMemo(() => {
    const byMarketplace = new Map<string, CatalogSkill[]>();
    for (const skill of matches()) {
      const known = byMarketplace.get(skill.marketplace);
      if (known === undefined) {
        byMarketplace.set(skill.marketplace, [skill]);
      } else {
        known.push(skill);
      }
    }
    return [...byMarketplace.entries()];
  });

  return (
    <>
      <header class={styles.sectionHead}>
        <A href={TOOLS} class={styles.backLink}>
          <ArrowLeft size={14} aria-hidden="true" />
          Back to Tools
        </A>
        <h2>Add a skill</h2>
        <p class={styles.lede}>
          Skills are folders of instructions an agent loads when it needs them. These come from
          plugin marketplaces on GitHub; flyco copies the one you pick into every session.
        </p>
      </header>

      <SearchField
        placeholder="Search skills"
        aria-label="Search skills"
        value={query()}
        onInput={(event) => setQuery(event.currentTarget.value)}
      />

      <ProblemNotice
        error={catalog.error}
        action={{ label: "Retry", onClick: () => void refetch() }}
      />

      <For each={catalog()?.failed ?? []}>
        {(problem) => (
          <p class={styles.noteProblem}>
            {problem.marketplace} could not be read: {problem.detail}
          </p>
        )}
      </For>

      <Show when={(catalog()?.pending ?? []).length > 0}>
        <p class={styles.note}>
          Reading {(catalog()?.pending ?? []).join(", ")}… this takes a few seconds the first time.
          <button type="button" class={styles.linkButton} onClick={() => void refetch()}>
            Check again
          </button>
        </p>
      </Show>

      <Show when={catalog.loading && catalog() === undefined}>
        <p class={styles.note}>Reading your marketplaces…</p>
      </Show>

      <For each={groups()}>
        {([marketplace, skills]) => (
          <div class={styles.group}>
            <p class={styles.groupLabel}>{marketplace}</p>
            <ul class={styles.rows} aria-label={`Skills in ${marketplace}`}>
              <For each={skills}>
                {(skill) => (
                  <li>
                    <button type="button" class={styles.row} onClick={() => props.onChoose(skill)}>
                      <span class={styles.rowBody}>
                        <span class={styles.rowTitle}>{skill.name}</span>
                        <span class={styles.rowMeta}>{skill.description}</span>
                      </span>
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </div>
        )}
      </For>

      <Show
        when={
          !catalog.loading &&
          catalog.error === undefined &&
          matches().length === 0 &&
          (catalog()?.pending ?? []).length === 0
        }
      >
        <p class={styles.note}>
          <Show when={query().trim() !== ""} fallback="Your marketplaces offer no skills yet.">
            No skill matches “{query().trim()}”.
          </Show>
        </p>
      </Show>

      <Marketplaces onChanged={() => void refetch()} />
    </>
  );
}

/* ── Page 2: which harness ────────────────────────────────────────────── */

function ScopePage(props: {
  skill: CatalogSkill;
  onBack: () => void;
  onInstalled: () => void;
}) {
  const [scopes, setScopes] = createSignal<SkillScope[]>([...EVERY_SCOPE]);
  const [busy, setBusy] = createSignal(false);
  const [failure, setFailure] = createSignal<unknown>(null);

  function toggle(scope: SkillScope): void {
    setScopes((chosen) =>
      chosen.includes(scope) ? chosen.filter((known) => known !== scope) : [...chosen, scope],
    );
  }

  async function install(): Promise<void> {
    setBusy(true);
    setFailure(null);
    try {
      await installCatalogSkill({
        marketplace: props.skill.marketplace,
        plugin: props.skill.plugin,
        name: props.skill.name,
        scopes: scopes(),
      });
      props.onInstalled();
    } catch (error) {
      setFailure(error);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <header class={styles.sectionHead}>
        <button type="button" class={cx(styles.backLink, styles.backButton)} onClick={props.onBack}>
          <ArrowLeft size={14} aria-hidden="true" />
          Back
        </button>
        <h2>{props.skill.name}</h2>
        <p class={styles.lede}>{props.skill.description}</p>
      </header>

      <div class={styles.field}>
        <span class={styles.fieldLabel} id="skill-scope-label">
          Which agents get it
        </span>
        <div class={styles.segmentedInline} role="group" aria-labelledby="skill-scope-label">
          <For each={EVERY_SCOPE}>
            {(scope) => (
              <button
                type="button"
                class={cx(styles.segment, scopes().includes(scope) && styles.segmentOn)}
                aria-pressed={scopes().includes(scope)}
                onClick={() => toggle(scope)}
              >
                {SCOPE_LABEL[scope]}
              </button>
            )}
          </For>
        </div>
        <p class={styles.hint}>
          The two read their skills from different directories, so a skill is installed for each one
          you pick.
        </p>
      </div>

      <ProblemNotice error={failure()} />
      <div class={styles.formActions}>
        <button
          type="button"
          class={styles.pillPrimary}
          disabled={busy() || scopes().length === 0}
          title={scopes().length === 0 ? "Pick at least one agent" : undefined}
          onClick={() => void install()}
        >
          {busy() ? "Adding…" : "Add skill"}
        </button>
      </div>
    </>
  );
}

/* ── The marketplaces themselves ──────────────────────────────────────── */

function Marketplaces(props: { onChanged: () => void }) {
  const [listed, { refetch }] = createQuery(listMarketplaces);
  const [adding, setAdding] = createSignal(false);
  const [repo, setRepo] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  // A refusal is drawn where the action was: a repository already added is
  // read under the field it was typed into, a removal above the row.
  const [addFailure, setAddFailure] = createSignal<unknown>(null);
  const [removeFailure, setRemoveFailure] = createSignal<unknown>(null);

  async function add(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setBusy(true);
    setAddFailure(null);
    try {
      await addMarketplace({ repo: repo().trim() });
      setRepo("");
      setAdding(false);
      await refetch();
      props.onChanged();
    } catch (error) {
      setAddFailure(error);
    } finally {
      setBusy(false);
    }
  }

  async function remove(marketplace: MarketplaceView): Promise<void> {
    const id = marketplace.id;
    // The built-in marketplace has no row and no id; its card carries no
    // Remove button, so this is a guard for the type rather than a case.
    if (id === null || id === undefined) {
      return;
    }
    setBusy(true);
    setRemoveFailure(null);
    try {
      await deleteMarketplace(id);
      await refetch();
      props.onChanged();
    } catch (error) {
      setRemoveFailure(error);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Marketplaces</p>
      <ProblemNotice error={listed.error ?? removeFailure()} />
      <ul class={styles.rows} aria-label="Marketplaces">
        <For each={listed() ?? []}>
          {(marketplace) => (
            <li class={cx(styles.row, styles.rowStatic)}>
              <span class={styles.rowBody}>
                <span class={styles.rowTitle}>{marketplace.repo}</span>
                <span class={styles.rowMeta}>
                  {marketplace.built_in
                    ? "Provided by flyco"
                    : `Added${marketplace.git_ref === null ? "" : ` · ${marketplace.git_ref}`}`}
                </span>
              </span>
              <Show when={!marketplace.built_in}>
                <button
                  type="button"
                  class={styles.iconButtonDanger}
                  aria-label={`Remove ${marketplace.repo}`}
                  disabled={busy()}
                  onClick={() => void remove(marketplace)}
                >
                  <Trash2 size={14} aria-hidden="true" />
                </button>
              </Show>
            </li>
          )}
        </For>
      </ul>

      <Show
        when={adding()}
        fallback={
          <div>
            <button type="button" class={styles.pill} onClick={() => setAdding(true)}>
              <Plus size={14} aria-hidden="true" />
              Add marketplace
            </button>
          </div>
        }
      >
        <form class={styles.form} onSubmit={(event) => void add(event)}>
          <div class={styles.field}>
            <label for="marketplace-repo">GitHub repository</label>
            <input
              id="marketplace-repo"
              value={repo()}
              onInput={(event) => setRepo(event.currentTarget.value)}
              placeholder="owner/name"
              required
            />
            <p class={styles.hint}>
              A repository with <code>.claude-plugin/marketplace.json</code> in it. flyco reads it
              with your GitHub account, so a private one works.
            </p>
          </div>
          <ProblemNotice error={addFailure()} />
          <div class={styles.formActions}>
            <button type="submit" class={styles.pillPrimary} disabled={busy()}>
              {busy() ? "Adding…" : "Add marketplace"}
            </button>
            <button
              type="button"
              class={styles.pill}
              disabled={busy()}
              onClick={() => setAdding(false)}
            >
              Cancel
            </button>
          </div>
        </form>
      </Show>
    </div>
  );
}
