'use client';
import React from 'react';

export function Input({className='',...rest}){return <input className={'clr-input '+className} {...rest}/>;}
export function Textarea({className='',...rest}){return <textarea className={'clr-textarea '+className} {...rest}></textarea>;}

export function Password({className='',...rest}){
  const [show,setShow]=React.useState(false);
  return <div className="clr-password-wrapper">
    <input type={show?'text':'password'} className={'clr-input '+className} {...rest}/>
    <button type="button" className="clr-password-toggle" aria-label="Show password" onClick={()=>setShow(s=>!s)}><clr-icon shape={show?'eye-hide':'eye'} size="16"></clr-icon></button>
  </div>;
}

export function FormField({label,helper,error,success,required,htmlFor,children,className=''}){
  const state=error?'clr-error':success?'clr-success':'';
  const sub=error||success||helper;
  return <div className={['clr-form-control',state,className].filter(Boolean).join(' ')}>
    {label&&<label className="clr-control-label" htmlFor={htmlFor}>{label}{required&&<span className="clr-required">*</span>}</label>}
    {children}
    {sub&&<span className="clr-subtext">{(error||success)&&<clr-icon shape={error?'exclamation-circle':'check-circle'} size="12" class="is-solid"></clr-icon>}{sub}</span>}
  </div>;
}

export function Select({options,children,className='',...rest}){
  return <div className="clr-select-wrapper"><select className={'clr-select '+className} {...rest}>
    {options?options.map(o=>typeof o==='string'?<option key={o} value={o}>{o}</option>:<option key={o.value} value={o.value} disabled={o.disabled}>{o.label}</option>):children}
  </select></div>;
}

/// Searchable multi-select: a select-styled trigger opening a dropdown
/// with a filter box and a checkbox per option. `values` is the array of
/// selected option values; `onChange` receives the new array.
export function MultiSelect({options,values,onChange,placeholder='Select…',disabled,maxHeight=220,className='',...rest}){
  const [open,setOpen]=React.useState(false);
  const [query,setQuery]=React.useState('');
  const rootRef=React.useRef(null);
  const searchRef=React.useRef(null);
  React.useEffect(()=>{
    if(!open)return;
    setQuery('');
    const onDown=e=>{if(rootRef.current&&!rootRef.current.contains(e.target))setOpen(false);};
    const onKey=e=>{if(e.key==='Escape')setOpen(false);};
    document.addEventListener('mousedown',onDown);
    document.addEventListener('keydown',onKey);
    const t=setTimeout(()=>searchRef.current&&searchRef.current.focus(),0);
    return()=>{document.removeEventListener('mousedown',onDown);document.removeEventListener('keydown',onKey);clearTimeout(t);};
  },[open]);
  const selected=new Set(values||[]);
  const toggle=v=>{
    const next=new Set(selected);
    next.has(v)?next.delete(v):next.add(v);
    onChange([...options.filter(o=>next.has(o.value)).map(o=>o.value)]);
  };
  const shown=options.filter(o=>!query||String(o.label).toLowerCase().includes(query.toLowerCase()));
  const summary=values&&values.length
    ?options.filter(o=>selected.has(o.value)).map(o=>o.label).join(', ')
    :'';
  return <div ref={rootRef} className={'clr-multiselect '+className} {...rest}>
    <button type="button" className="clr-select clr-multiselect-trigger" disabled={disabled}
      onClick={()=>setOpen(o=>!o)} aria-expanded={open}>
      <span className={summary?'':'clr-multiselect-placeholder'}>{summary||placeholder}</span>
    </button>
    {open&&<div className="clr-multiselect-menu">
      <input ref={searchRef} className="clr-input clr-multiselect-search" value={query}
        placeholder="Search…" onChange={e=>setQuery(e.target.value)}/>
      <div className="clr-multiselect-options" style={{maxHeight}}>
        {shown.map(o=><label key={o.value} className={'clr-multiselect-option'+(o.disabled?' disabled':'')}>
          <input type="checkbox" checked={selected.has(o.value)} disabled={o.disabled}
            onChange={()=>toggle(o.value)}/>
          <span>{o.label}</span>
        </label>)}
        {shown.length===0&&<div className="clr-multiselect-empty">No matches.</div>}
      </div>
      {values&&values.length>0&&<div className="clr-multiselect-footer">
        <button type="button" onClick={()=>onChange([])}>Clear</button>
        <span>{values.length} selected</span>
      </div>}
    </div>}
  </div>;
}

export function Checkbox({label,indeterminate,className='',...rest}){
  const ref=React.useRef();
  React.useEffect(()=>{if(ref.current)ref.current.indeterminate=!!indeterminate;},[indeterminate]);
  const id=React.useId();
  return <div className={'clr-checkbox-wrapper '+className}>
    <input ref={ref} type="checkbox" id={id} className={indeterminate?'indeterminate':''} {...rest}/>
    {label&&<label htmlFor={id}>{label}</label>}
  </div>;
}
