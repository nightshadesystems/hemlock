'use client';
import React from 'react';
export function Header({logo,title='Nightshade',nav=[],onNavigate,search,actions,children,brand,className=''}){
  return <header className={'header '+className}>
    <div className="branding">{logo&&<img src={logo} alt=""/>}{title&&<span className="title">{title}</span>}{!title&&brand!==false&&<span className="title">Nightshade</span>}</div>
    {(nav.length>0||children)&&<span className="header-divider"></span>}
    {children}
    {nav.length>0&&<nav className="header-nav">{nav.map((n,i)=><a key={i} href={n.href||'#'} className={'nav-link'+(n.active?' active':'')} onClick={e=>{if(onNavigate){e.preventDefault();onNavigate(n);}}}>{n.label}</a>)}</nav>}
    <div className="header-actions">
      {search&&<div className="search"><clr-icon shape="search" size="14"></clr-icon><input placeholder={typeof search==='string'?search:'Search…'}/></div>}
      {actions}
    </div>
  </header>;
}
export function HeaderDivider(){return <span className="header-divider"></span>;}
export function HeaderDropdown({label,value,items=[],onSelect}){
  const [open,setOpen]=React.useState(false);
  const [val,setVal]=React.useState(value);
  const ref=React.useRef();
  React.useEffect(()=>{
    const h=e=>{if(ref.current&&!ref.current.contains(e.target))setOpen(false);};
    document.addEventListener('mousedown',h);return ()=>document.removeEventListener('mousedown',h);
  },[]);
  return <div className="header-dropdown" ref={ref}>
    <button onClick={()=>setOpen(o=>!o)} aria-expanded={open}>
      <span className="hd-text">{label&&<span className="hd-label">{label}</span>}<span className="hd-value">{val}</span></span>
      <clr-icon shape="angle" dir="down" size="12"></clr-icon>
    </button>
    {open&&<div className="dropdown-menu">{items.map((it,i)=><button key={i} className="dropdown-item" onClick={()=>{setVal(it);setOpen(false);onSelect&&onSelect(it);}}>{it}</button>)}</div>}
  </div>;
}
export function HeaderAction({icon,badge,label,onClick,title}){
  return <button className="nav-icon" aria-label={label} title={title||label} onClick={onClick} style={{position:'relative'}}>
    <clr-icon shape={icon} size="20"></clr-icon>
    {badge!=null&&<span className="badge badge-danger" style={{position:'absolute',top:4,right:2}}>{badge}</span>}
  </button>;
}
export function Subnav({items=[],onNavigate,className=''}){
  return <nav className={'subnav '+className}>{items.map((n,i)=><a key={i} href={n.href||'#'} className={'nav-link'+(n.active?' active':'')} onClick={e=>{if(onNavigate){e.preventDefault();onNavigate(n);}}}>{n.label}</a>)}</nav>;
}
