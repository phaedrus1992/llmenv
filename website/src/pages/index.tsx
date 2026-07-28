import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import styles from './index.module.css';

type Feature = {
  title: string;
  description: string;
};

const FEATURES: Feature[] = [
  {
    title: 'Per-environment config',
    description:
      'Scopes detect network, host, user, and project context; tags attach config to the scopes that need it. A work MCP server or a personal plugin activates automatically — no global config bleeding into repos it doesn’t belong in.',
  },
  {
    title: 'One config, every tool',
    description:
      'Capabilities — MCP, LSP, hooks, permissions, plugins — are declared once in an engine-neutral format. Adapters translate that into Claude Code, Crush, Opencode, and others, so switching tools doesn’t mean rewriting config.',
  },
  {
    title: 'Integrated memory',
    description:
      'ICM gives agents durable, queryable memory across sessions — decisions and prior fixes survive /clear and new sessions. Codebase memory indexes your repo so agents search a knowledge graph instead of re-reading files cold.',
  },
  {
    title: 'Context management',
    description:
      'Built-in guards keep long sessions on track: loop detection catches a model stuck repeating itself, a task tracker survives compaction, and context-mode offloads large tool output to a sandbox instead of bloating the conversation.',
  },
  {
    title: 'First-class MCP & LSP',
    description:
      'MCP servers and language servers are top-level config, not an afterthought. Declare them once and every adapter renders the engine-specific wiring — transports, permission scoping — for you.',
  },
  {
    title: 'Plugins & skills, one bundle',
    description:
      'Bundle skills, commands, agents, and MCP servers together and activate them by tag. llmenv translates plugin content across engines, so a Claude Code plugin still works when a teammate is on Crush.',
  },
];

function Hero() {
  const { siteConfig } = useDocusaurusContext();
  return (
    <header className={styles.hero}>
      <div className="container">
        <h1 className={styles.title}>{siteConfig.title}</h1>
        <p className={styles.tagline}>{siteConfig.tagline}</p>
        <p className={styles.description}>
          A single global agent config can&apos;t express &ldquo;use the office MCP server only
          at work&rdquo; or &ldquo;load these plugins only in this repo&rdquo;. llmenv lets you
          declare configuration once, attach it to <strong>scopes</strong> via{' '}
          <strong>tags</strong>, and have the right slice activate automatically — from a shell
          hook that fires on every prompt.
        </p>
        <div className={styles.buttons}>
          <Link className="button button--primary button--lg" to="/docs/getting-started">
            Get Started
          </Link>
          <Link className="button button--secondary button--lg" to="/docs/philosophy">
            Why llmenv?
          </Link>
        </div>
      </div>
    </header>
  );
}

function Features() {
  return (
    <section className={styles.features}>
      <div className="container">
        <h2 className={styles.featuresTitle}>Why llmenv</h2>
        <div className={styles.featureList}>
          {FEATURES.map((feature) => (
            <div key={feature.title}>
              <h3 className={styles.featureTitle}>{feature.title}</h3>
              <p className={styles.featureDesc}>{feature.description}</p>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

export default function Home() {
  return (
    <Layout description="direnv for Claude Code and other AI tools.">
      <Hero />
      <Features />
    </Layout>
  );
}
