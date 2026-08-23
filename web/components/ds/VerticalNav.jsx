'use client';
import React from 'react';
export function VerticalNav({groups=[],collapsible,defaultCollapsed,activeId,onNavigate,className='',style}){
  const [collapsed,setCollapsed]=React.useState(!!defaultCollapsed);
  const [active,setActive]=React.useState(activeId);
  React.useEffect(()=>{setActive(activeId);},[activeId]);
  return <nav className={['clr-vertical-nav',collapsed?'is-collapsed':'',className].filter(Boolean).join(' ')} style={style}>
    {collapsible&&<button className="clr-vertical-nav-trigger" aria-label={collapsed?'Expand':'Collapse'} onClick={()=>setCollapsed(c=>!c)}><clr-icon shape="angle-double" dir={collapsed?'right':'left'} size="14"></clr-icon></button>}
    {groups.map((g,gi)=><React.Fragment key={gi}>
      {g.label&&!collapsed&&<div className="nav-group-text">{g.label}</div>}
      {g.items.map(it=><a key={it.id||it.label} href={it.href||'#'} title={collapsed?it.label:undefined}
        className={'nav-link'+((active?active===it.id:it.active)?' active':'')}
        onClick={e=>{e.preventDefault();setActive(it.id);onNavigate&&onNavigate(it);}}>
        {it.icon&&<clr-icon shape={it.icon} size="16"></clr-icon>}
        {!collapsed&&<span>{it.label}</span>}
        {!collapsed&&it.badge!=null&&<span className={'badge nav-badge'+(it.badgeStatus?' badge-'+it.badgeStatus:'')}>{it.badge}</span>}
      </a>)}
    </React.Fragment>)}
  </nav>;
}
