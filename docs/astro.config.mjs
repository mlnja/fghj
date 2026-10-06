import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import remarkGfm from 'remark-gfm';

export default defineConfig({
  // Astro 6's MDX pipeline doesn't apply GFM the way the plain-Markdown one
  // does, so a table in `index.mdx` renders as literal pipes. Declaring the
  // plugin here covers both.
  markdown: { remarkPlugins: [remarkGfm] },
  integrations: [
    mermaid({
      theme: 'neutral',
      autoTheme: true,
      // Colours stay with the auto-selected light/dark theme (one set of
      // `themeVariables` cannot serve both), but the typeface can follow the
      // rest of the site — see `src/styles/fghj-tokens.css`.
      mermaidConfig: {
        fontFamily: "'Space Grotesk', 'Helvetica Neue', Arial, sans-serif",
      },
    }),
    starlight({
      title: 'fghj',
      description: 'Local development orchestration for multi-repo user flows',
      favicon: '/favicon.svg',
      logo: { src: './src/assets/fghj-mark.svg', alt: '' },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/mlnja/fghj' }],

      // The three faces of the fghj design system, from the same Google Fonts
      // request `ui/src/app.css` makes — so the docs and the web UI render in
      // literally the same type.
      head: [
        {
          tag: 'link',
          attrs: { rel: 'preconnect', href: 'https://fonts.googleapis.com' },
        },
        {
          tag: 'link',
          attrs: { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: true },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'stylesheet',
            href: 'https://fonts.googleapis.com/css2?family=Podkova:wght@400..800&family=Space+Grotesk:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500;600;700&display=swap',
          },
        },
      ],

      customCss: ['./src/styles/fghj-tokens.css', './src/styles/fghj-theme.css'],

      expressiveCode: {
        themes: ['github-light', 'github-dark'],
        // Frames borrow the site's own hairline/panel tokens so a code block
        // reads as one of the UI's panels rather than a foreign widget.
        styleOverrides: {
          borderColor: 'var(--sl-color-hairline)',
          borderRadius: 'var(--fghj-radius-md)',
          codeBackground: 'var(--fghj-code-bg)',
          codeFontFamily: 'var(--sl-font-mono)',
          codeFontSize: '0.8125rem',
          uiFontFamily: 'var(--sl-font-mono)',
          frames: {
            editorTabBarBackground: 'var(--sl-color-bg-nav)',
            editorActiveTabBackground: 'var(--fghj-code-bg)',
            editorActiveTabIndicatorTopColor: 'var(--sl-color-accent)',
            terminalBackground: 'var(--fghj-code-bg)',
            terminalTitlebarBackground: 'var(--sl-color-bg-nav)',
            frameBoxShadowCssValue: 'none',
          },
        },
      },

      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', slug: 'getting-started/introduction' },
            { label: 'Installation', slug: 'getting-started/installation' },
            { label: 'Quickstart', slug: 'getting-started/quickstart' },
          ],
        },
        {
          label: 'Tutorial',
          items: [
            { label: 'Start here', slug: 'tutorial' },
            { label: '1. One service', slug: 'tutorial/01-one-service' },
            { label: '2. A database', slug: 'tutorial/02-a-database' },
            { label: '3. A migration', slug: 'tutorial/03-a-migration' },
            { label: '4. A second repo', slug: 'tutorial/04-a-second-repo' },
            { label: '5. Flows', slug: 'tutorial/05-flows' },
            { label: '6. When it goes wrong', slug: 'tutorial/06-when-it-goes-wrong' },
            { label: '7. Attaching a debugger', slug: 'tutorial/07-attaching-a-debugger' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Architecture', slug: 'concepts/architecture' },
            { label: 'Flat workspace model', slug: 'concepts/flat-workspace-model' },
            { label: 'Branch ownership model', slug: 'concepts/branch-ownership-model' },
            { label: 'Fog-of-war visibility', slug: 'concepts/fog-of-war-visibility' },
            { label: 'Node identity & domains', slug: 'concepts/node-identity-and-domains' },
            { label: 'Terminating nodes', slug: 'concepts/terminating-nodes' },
            { label: 'Local CA & TLS proxy', slug: 'concepts/local-ca-and-tls-proxy' },
            { label: 'In-network TLS proxy sidecar', slug: 'concepts/sidecar' },
            { label: 'Split DNS', slug: 'concepts/split-dns' },
            { label: 'Run lifecycle & registry', slug: 'concepts/run-lifecycle-and-registry' },
            { label: 'Concurrency model', slug: 'concepts/concurrency-model' },
            { label: 'Persistence & workspace store', slug: 'concepts/persistence-and-workspace-store' },
            { label: 'Docker & downloads', slug: 'concepts/docker-and-downloads' },
            { label: 'Control API', slug: 'concepts/control-api' },
            { label: 'UI architecture', slug: 'concepts/ui-architecture' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'What fghj fixes', slug: 'guides/what-it-fixes' },
            { label: 'fghj and OrbStack', slug: 'guides/vs-orbstack' },
            { label: 'HTTP vs. raw: choosing a zone', slug: 'guides/networking-http-vs-raw' },
            { label: 'Attaching a debugger', slug: 'guides/debugging' },
            { label: 'Python services in fghj', slug: 'guides/python' },
            { label: 'Go services in fghj', slug: 'guides/go' },
            { label: 'What fghj touches, and how to remove it', slug: 'guides/uninstalling' },
          ],
        },
        {
          label: 'CLI Reference',
          items: [
            { label: 'fghj validate', slug: 'cli/validate' },
            { label: 'fghj graph', slug: 'cli/graph' },
            { label: 'fghj wire', slug: 'cli/wire' },
            { label: 'fghj exec', slug: 'cli/exec' },
            { label: 'fghj daemon', slug: 'cli/daemon' },
            { label: 'fghj doctor', slug: 'cli/doctor' },
            { label: 'fghj uninstall', slug: 'cli/uninstall' },
            { label: 'fghjd (superdaemon)', slug: 'cli/fghjd' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'fghj.yaml', slug: 'reference/fghj-yaml' },
            // `reference/runs` is deliberately not listed (it sets
            // `sidebar.hidden`). A named run varies only identity, volume
            // freshness and which nodes are up — never the source or the
            // config — so it is a mechanism to document, not a feature to
            // send a reader looking for. The pages that need it link to it.
          ],
        },
      ],
    }),
  ],
});
