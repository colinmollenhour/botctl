import clsx from 'clsx';
import Heading from '@theme/Heading';

const FeatureList = [
  {
    eyebrow: 'Dashboard Popup',
    title: 'Keep a live control panel one keypress away',
    description: (
      <>
        Launch the dashboard in a tmux popup, keep it alive with <code>--persistent</code>,
        and jump straight into the Claude pane you care about without losing the bigger picture.
      </>
    ),
  },
  {
    eyebrow: 'YOLO',
    title: 'Let botctl babysit the boring parts',
    description: (
      <>
        Turn on <code>yolo</code> for one pane or one workspace and let botctl handle the
        repetitive safe confirmation flows while keeping the guardrails intact.
      </>
    ),
  },
  {
    eyebrow: 'Serve',
    title: 'Stream live session state for humans or tooling',
    description: (
      <>
        Run <code>serve</code> when you want a live event stream, use human output for
        watching, or switch to JSONL when you want to wire botctl into something bigger.
      </>
    ),
  },
];

function Feature({eyebrow, title, description}) {
  return (
    <div className={clsx('col col--4', 'feature-grid__item')}>
      <div className="feature-card">
        <p className="feature-card__eyebrow">{eyebrow}</p>
        <Heading as="h3" className="feature-card__title">{title}</Heading>
        <p className="feature-card__description">{description}</p>
      </div>
    </div>
  );
}

export default function HomepageFeatures() {
  return (
    <section className="feature-grid">
      <div className="container">
        <div className="feature-grid__header">
          <p className="feature-grid__label">Three ways to break out of the TUI</p>
          <Heading as="h2">The useful stuff: popup dashboard, yolo babysitting, and live session streaming</Heading>
          <p>
            botctl is most fun when it stops feeling like a pile of terminal panes and starts
            feeling like something you can actually steer.
          </p>
        </div>
        <div className="row">
          {FeatureList.map((props, idx) => (
            <Feature key={idx} {...props} />
          ))}
        </div>
      </div>
    </section>
  );
}
