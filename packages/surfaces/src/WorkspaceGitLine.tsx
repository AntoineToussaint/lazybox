import { Icon } from './Icons';
import type { RowGit } from './workspace-row-model';
import './workspace-git.css';

/**
 * A workspace row's git line: the branch, then what needs doing about it —
 * uncommitted files, commits not pushed, commits to pull, commits behind the
 * default branch — and its pull request with a dot for its checks. One line
 * of fixed height whatever it holds, so the rows stay a grid. The tooltip
 * spells out every repository; screen readers get the same in words.
 */
export function GitLine({ git, fallback, empty = false }: { git?: RowGit; fallback?: { name: string; detail: string; title: string }; empty?: boolean }) {
  // Nothing to track yet: the line keeps its slot, quietly.
  if (empty) return <small className="workspace-git is-pending" data-git-line="">
    <Icon name="branch" size={11}/>
    <span className="workspace-git-branch">No branch yet</span>
  </small>;
  if (!git) {
    // Before the Runner answers (or an older Runner): the branch the workspace was made on.
    return <small className="workspace-git is-pending" title={fallback?.title}>
      <Icon name="branch" size={11}/>
      <span className="workspace-git-branch">{fallback?.name || 'Reading git state…'}</span>
      {/* "+1 branch" reads "+1", as the answered line does; the repository count is the line above. */}
      {fallback?.detail.startsWith('+') && <span className="workspace-git-more">{fallback.detail.replace(/ branch(es)?$/, '')}</span>}
    </small>;
  }
  return <small className={`workspace-git${git.problem ? ' has-problem' : ''}`} title={git.title} data-git-line="">
    <span className="sr-only">{git.label}</span>
    <Icon name="branch" size={11}/>
    <span className="workspace-git-branch" aria-hidden="true">{git.branch || 'No branch'}</span>
    {git.more && <span className="workspace-git-more" aria-hidden="true" title={`${git.more.slice(1)} more ${git.more === '+1' ? 'branch' : 'branches'}`}>{git.more}</span>}
    <span className="workspace-git-badges" aria-hidden="true">
      {git.badges.map(badge => <span key={badge.kind} className={`git-badge is-${badge.kind}`} data-git-badge={badge.kind}>
        {badge.kind === 'dirty' && <i className="git-dot"/>}{badge.text}
      </span>)}
      {git.pulls.slice(0, 2).map(pull => pull.number ? <span key={pull.member + pull.number} className={`git-pull is-${pull.tone}`} data-git-pull={pull.tone}>
        <i className="git-dot"/>{pull.text}
      </span> : <span key="more" className="git-pull is-more">{pull.text}</span>)}
      {git.pulls.length > 2 && <span className="git-pull is-more">+{git.pulls.length - 2}</span>}
      {git.problem && <span className="git-badge is-problem"><Icon name="alert" size={10}/></span>}
    </span>
  </small>;
}
