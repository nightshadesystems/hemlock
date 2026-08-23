'use client';
import React from 'react';

export function Card({header,footer,clickable,className='',children,onClick,...rest}){
  const cls=['card',clickable?'clickable':'',className].filter(Boolean).join(' ');
  return <div className={cls} onClick={onClick} {...rest}>
    {header&&<div className="card-header">{header}</div>}
    {children}
    {footer&&<div className="card-footer">{footer}</div>}
  </div>;
}
export function CardBlock({title,text,className='',children}){
  return <div className={'card-block '+className}>{title&&<div className="card-title">{title}</div>}{text&&<div className="card-text">{text}</div>}{children}</div>;
}

export function Label({status,accent,clickable,dismissable,onDismiss,badge,className='',children,...rest}){
  const cls=['label',status?'label-'+status:'',accent?'label-accent':'',clickable?'clickable':'',className].filter(Boolean).join(' ');
  return <span className={cls} {...rest}>{children}{badge!=null&&<Badge>{badge}</Badge>}{dismissable&&<button className="label-dismiss" aria-label="Dismiss" onClick={onDismiss}>×</button>}</span>;
}
export function Badge({status,accent,className='',children,...rest}){
  const cls=['badge',status?'badge-'+status:'',accent?'badge-accent':'',className].filter(Boolean).join(' ');
  return <span className={cls} {...rest}>{children}</span>;
}

const alertShapes={info:'info-circle',success:'check-circle',warning:'exclamation-triangle',danger:'exclamation-circle'};
export function Alert({status='info',appLevel,sm,closable,onClose,actions,items,children,className=''}){
  const [closed,setClosed]=React.useState(false);
  if(closed)return null;
  const cls=['alert','alert-'+status,appLevel?'alert-app-level':'',sm?'alert-sm':'',className].filter(Boolean).join(' ');
  const close=()=>{setClosed(true);onClose&&onClose();};
  const body=items?<div className="alert-items">{items.map((it,i)=><div key={i} className="alert-item"><span className="alert-text">{it}</span></div>)}</div>:<span className="alert-text">{children}</span>;
  return <div className={cls} role="alert">
    <clr-icon class="alert-icon is-solid" shape={alertShapes[status]} size={sm?14:16}></clr-icon>
    {body}
    {actions&&<div className="alert-actions">{actions.map((a,i)=><button key={i} className="alert-action" onClick={a.onClick}>{a.label}</button>)}</div>}
    {closable&&<button className="close" aria-label="Close" onClick={close}>×</button>}
  </div>;
}

export function Icon({shape,size=16,solid,dir,className='',style,...rest}){
  const cls=[solid?'is-solid':'',className].filter(Boolean).join(' ');
  return <clr-icon shape={shape} size={String(size)} class={cls||undefined} dir={dir} style={style} {...rest}></clr-icon>;
}

export function Spinner({size='md',inline,className=''}){
  const cls=['spinner',size==='sm'?'spinner-sm':size==='md'?'spinner-md':'',inline?'spinner-inline':'',className].filter(Boolean).join(' ');
  return <span className={cls}></span>;
}
