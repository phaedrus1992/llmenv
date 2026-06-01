import type { ReactNode } from 'react';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import styles from './index.module.css';

type PipelineStep = {
  label: string;
  description: string;
};

const PIPELINE_STEPS: PipelineStep[] = [
  { label: 'Scopes', description: 'Detect context: network, host, user, project.' },
  { label: 'Tags', description: 'Active scopes contribute tags to the active set.' },
  { label: 'Bundles', description: 'MCP servers, plugins, memory fire on tag match.' },
  { label: 'Materialize', description: 'Merge into a content-hashed config directory.' },
  { label: 'Emit', description: 'Adapter writes agent-native config files.' },
];

function Hero(): ReactNode {
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

function Pipeline(): ReactNode {
  return (
    <section className={styles.pipeline}>
      <div className="container">
        <h2 className={styles.pipelineTitle}>One fixed pipeline</h2>
        <p className={styles.pipelineSub}>
          Every environment change passes through the same deterministic stages.
        </p>
        <div className={styles.steps}>
          {PIPELINE_STEPS.map((step, i) => (
            <>
              <div key={step.label} className={styles.step}>
                <div className={styles.stepLabel}>{step.label}</div>
                <p className={styles.stepDesc}>{step.description}</p>
              </div>
              {i < PIPELINE_STEPS.length - 1 && (
                <span key={`arrow-${i}`} className={styles.stepArrow} aria-hidden>
                  →
                </span>
              )}
            </>
          ))}
        </div>
      </div>
    </section>
  );
}

export default function Home(): ReactNode {
  return (
    <Layout description="direnv for Claude Code and other AI tools.">
      <Hero />
      <Pipeline />
    </Layout>
  );
}
