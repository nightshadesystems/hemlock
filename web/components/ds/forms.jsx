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
    {options?options.map(o=>typeof o==='string'?<option key={o} value={o}>{o}</option>:<option key={o.value} value={o.value}>{o.label}</option>):children}
  </select></div>;
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
