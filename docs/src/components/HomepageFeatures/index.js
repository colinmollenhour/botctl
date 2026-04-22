import clsx from 'clsx';
import Heading from '@theme/Heading';

const FeatureList = [
  {
    eyebrow: 'Session Manager',
    title: 'Launch and target tmux panes explicitly',
    description: (
      <>
        Start managed Claude sessions, inspect pane metadata, and keep automation pinned
        to concrete tmux pane IDs instead of vague session guesses.
      </>
    ),
  },
  {
    eyebrow: 'Observer',
    title: 'Watch live terminal state without pretending it is easy',
    description: (
      <>
        Blend control-mode events with capture-backed reconciliation so diagnostics stay
        fast, but final decisions still come from the most trustworthy pane evidence.
      </>
    ),
  },
  {
    eyebrow: 'Classifier',
    title: 'Prefer Unknown over sending the wrong key',
    description: (
      <>
        Map Claude UI frames into explicit states like <code>ChatReady</code>,
        <code>PermissionDialog</code>, and <code>FolderTrustPrompt</code> while keeping
        ambiguous flows on the safe side.
      </>
    ),
  },
  {
    eyebrow: 'Automation',
    title: 'Guarded workflows, not blind send-keys macros',
    description: (
      <>
        Submit prompts, approve permissions, reject dialogs, dismiss surveys, and run
        recovery loops only when ownership, keybindings, and state validation all line up.
      </>
    ),
  },
  {
    eyebrow: 'Prompt Handoff',
    title: 'Stage prompts in SQLite before Claude asks for an editor',
    description: (
      <>
        Keep prepared prompts in <code>state.db</code>, bridge them into Claude&apos;s
        external-editor flow, and verify transitions instead of assuming submission worked.
      </>
    ),
  },
  {
    eyebrow: 'Fixtures',
    title: 'Replay captured UI cases when Claude changes on you',
    description: (
      <>
        Record fixture cases from live sessions and replay them in tests so classifier and
        workflow behavior stays explainable as Claude&apos;s terminal UI evolves.
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
          <p className="feature-grid__label">Why botctl exists</p>
          <Heading as="h2">Automation for Claude sessions that acts like a paranoid operator</Heading>
          <p>
            botctl is built around one rule: terminal automation is only safe when transport,
            observation, classification, and policy stay separate.
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
