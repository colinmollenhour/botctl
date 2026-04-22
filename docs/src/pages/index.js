import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';

import HomepageFeatures from '@site/src/components/HomepageFeatures';

function HomepageHeader() {
  const {siteConfig} = useDocusaurusContext();

  return (
    <header className="hero-botctl">
      <div className="hero-botctl__backdrop" />
      <div className="hero-botctl__glow hero-botctl__glow--cyan" />
      <div className="hero-botctl__glow hero-botctl__glow--amber" />
      <div className="hero-botctl__grid" />
      <div className="hero-botctl__content">
        <p className="hero-botctl__eyebrow">Terminal control, minus the cowboy energy</p>
        <Heading as="h1" className="hero-botctl__title">
          {siteConfig.title}
        </Heading>
        <p className="hero-botctl__subtitle">{siteConfig.tagline}</p>
        <p className="hero-botctl__body">
          Launch Claude in tmux, observe the live pane state, and automate only when
          the classifier, keybindings, and workflow guardrails all agree.
        </p>
        <div className="hero-botctl__actions">
          <Link className="button button--primary button--lg hero-botctl__button" to="/docs/">
            Read the Docs
          </Link>
          <Link className="button button--secondary button--lg hero-botctl__button hero-botctl__button--ghost" to="/docs/getting-started">
            First Successful Run
          </Link>
        </div>
      </div>
    </header>
  );
}

export default function Home() {
  const {siteConfig} = useDocusaurusContext();

  return (
    <Layout title="Home" description="Safe tmux automation for Claude Code sessions with explicit targeting, conservative classification, and guarded workflows.">
      <HomepageHeader />
      <main>
        <HomepageFeatures />
      </main>
    </Layout>
  );
}
