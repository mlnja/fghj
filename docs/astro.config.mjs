import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  integrations: [
    starlight({
      title: 'fghj',
      description: 'Local development orchestration for multi-repo user flows',
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/virviil/fghj' }],
      customCss: ['./src/styles/custom.css'],
      expressiveCode: { themes: ['github-light', 'github-dark'] },

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
          label: 'Concepts',
          items: [
            { label: 'Architecture', slug: 'concepts/architecture' },
            { label: 'Flat workspace model', slug: 'concepts/flat-workspace-model' },
            { label: 'Branch ownership model', slug: 'concepts/branch-ownership-model' },
            { label: 'Fog-of-war visibility', slug: 'concepts/fog-of-war-visibility' },
            { label: 'Node identity & domains', slug: 'concepts/node-identity-and-domains' },
            { label: 'Local CA & TLS proxy', slug: 'concepts/local-ca-and-tls-proxy' },
            { label: 'Split DNS', slug: 'concepts/split-dns' },
            { label: 'Run lifecycle & registry', slug: 'concepts/run-lifecycle-and-registry' },
            { label: 'Persistence & workspace store', slug: 'concepts/persistence-and-workspace-store' },
            { label: 'Docker & downloads', slug: 'concepts/docker-and-downloads' },
            { label: 'Control API', slug: 'concepts/control-api' },
            { label: 'UI architecture', slug: 'concepts/ui-architecture' },
          ],
        },
        {
          label: 'CLI Reference',
          items: [
            { label: 'fghj validate', slug: 'cli/validate' },
            { label: 'fghj graph', slug: 'cli/graph' },
            { label: 'fghj wire', slug: 'cli/wire' },
            { label: 'fghj daemon stop', slug: 'cli/daemon-stop' },
            { label: 'fghjd (superdaemon)', slug: 'cli/fghjd' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'fghj.yaml', slug: 'reference/fghj-yaml' },
          ],
        },
      ],
    }),
  ],
});
