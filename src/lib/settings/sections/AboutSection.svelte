<script lang="ts">
  /// Settings → About (#129 (c)). The first section extracted out of
  /// `SettingsApp.svelte`, and the smallest: read-only prose with no settings
  /// state at all, so it is the section that proves the seam rather than the
  /// one that tests it.
  ///
  /// No `settings-chrome.css` import: the sheet is keyed on the
  /// `.settings-chrome` class the parent puts on `.root`, and this renders
  /// inside that element, so `section` / `h2` / `a` pick the chrome rules up
  /// through the DOM. Only the rules that name `.about-list` — which Svelte
  /// scopes to whichever component holds the markup — travelled here with it.
  import { version as appVersion } from '../../../../package.json';

  /// The repository the About page links at. Moved with the markup: the parent
  /// held it as a top-level const and nothing else read it.
  const REPO_URL = 'https://github.com/Dyserna/cImp';
</script>

<section class="about-section">
  <h2>About</h2>
  <dl class="about-list">
    <dt>Author</dt>
    <dd>Amir Amashe</dd>

    <dt>Version</dt>
    <dd><code>{appVersion}</code></dd>

    <dt>Repository</dt>
    <dd>
      <a href={REPO_URL} target="_blank" rel="noopener noreferrer">
        {REPO_URL}
      </a>
    </dd>
  </dl>
</section>

<style>
  /* About page: a small definition list keyed by Author / Version /
     Repository. Two-column grid (label | value) so the values line up
     even with mixed key lengths. */
  .about-list {
    display: grid;
    grid-template-columns: max-content 1fr;
    column-gap: var(--space-4);
    row-gap: var(--space-3);
    margin: 0;
    padding: 0;
  }
  .about-list dt {
    color: var(--text-quiet-strong);
    font-size: var(--font-size-sm);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-weight: 600;
    padding-top: 2px;
  }
  .about-list dd {
    margin: 0;
    color: var(--text-primary);
    font-size: var(--font-size-md);
    word-break: break-all;
  }
  .about-list dd code {
    background: var(--surface-deep);
    border: 1px solid var(--border-subtle);
    padding: 1px var(--space-2);
    border-radius: var(--radius-sm);
    font-family: Consolas, Menlo, monospace;
    font-size: var(--font-size-sm);
  }
  .about-list dd a {
    color: var(--accent-purple);
    text-decoration: none;
    transition: color var(--motion-fast) var(--easing-standard);
  }
  .about-list dd a:hover {
    color: var(--accent-bright);
    text-decoration: underline;
  }
</style>
