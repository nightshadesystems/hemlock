'use client';
import React from 'react';
export function Datagrid({columns,rows,rowKey,selectable,expandable,renderDetail,pageSize=0,actionBar,placeholder='No items found.',footerText,compact,className=''}){
  const [sort,setSort]=React.useState(null);
  const [sel,setSel]=React.useState(()=>new Set());
  const [open,setOpen]=React.useState(()=>new Set());
  const [page,setPage]=React.useState(0);
  const keyOf=rowKey||((r,i)=>i);
  let data=rows.map((r,i)=>[keyOf(r,i),r]);
  if(sort){
    const col=columns.find(c=>c.key===sort.key);
    const dir=sort.dir==='asc'?1:-1;
    const cmp=col&&col.compare?col.compare:(x,y)=>{const a=x[sort.key],b=y[sort.key];return a>b?1:a<b?-1:0;};
    data.sort(([,a],[,b])=>cmp(a,b)*dir);
  }
  const pages=pageSize?Math.max(1,Math.ceil(data.length/pageSize)):1;
  const view=pageSize?data.slice(page*pageSize,(page+1)*pageSize):data;
  const nCols=columns.length+(selectable?1:0)+(expandable?1:0);
  const toggleSort=c=>{if(!c.sortable)return;setSort(s=>s&&s.key===c.key?{key:c.key,dir:s.dir==='asc'?'desc':'asc'}:{key:c.key,dir:'asc'});};
  const allSel=view.length>0&&view.every(([k])=>sel.has(k));
  const toggleAll=()=>setSel(s=>{const n=new Set(s);view.forEach(([k])=>{allSel?n.delete(k):n.add(k);});return n;});
  const toggleRow=k=>setSel(s=>{const n=new Set(s);n.has(k)?n.delete(k):n.add(k);return n;});
  const toggleOpen=k=>setOpen(s=>{const n=new Set(s);n.has(k)?n.delete(k):n.add(k);return n;});
  const clearSel=()=>setSel(new Set());
  const rh=compact?'var(--clr-base-dg-compact-row-height)':undefined;
  return <div className={'datagrid '+className} style={{fontSize:compact?12:undefined}}>
    {actionBar&&<div className="datagrid-action-bar">{actionBar({selected:sel,clear:clearSel})}</div>}
    <table className="datagrid-table">
      <thead><tr className="datagrid-row">
        {selectable&&<th className="datagrid-column datagrid-select"><div className="clr-checkbox-wrapper"><input type="checkbox" checked={allSel} onChange={toggleAll} aria-label="Select all"/></div></th>}
        {expandable&&<th className="datagrid-column datagrid-expandable-caret"></th>}
        {columns.map(c=><th key={c.key} className="datagrid-column" style={{width:c.width}}>
          {c.sortable?<button onClick={()=>toggleSort(c)}>{c.label}{sort&&sort.key===c.key&&<span className="sort-icon">{sort.dir==='asc'?'▲':'▼'}</span>}</button>:c.label}
        </th>)}
      </tr></thead>
      <tbody>
      {view.map(([k,r])=>{const isOpen=open.has(k);
        return <React.Fragment key={k}>
        <tr className={'datagrid-row'+(sel.has(k)?' datagrid-selected':'')}>
          {selectable&&<td className="datagrid-cell datagrid-select" style={{height:rh}}><div className="clr-checkbox-wrapper"><input type="checkbox" checked={sel.has(k)} onChange={()=>toggleRow(k)} aria-label="Select row"/></div></td>}
          {expandable&&<td className="datagrid-cell datagrid-expandable-caret" style={{height:rh}}><button onClick={()=>toggleOpen(k)} aria-label="Expand"><clr-icon shape="angle" dir={isOpen?'down':'right'} size="12"></clr-icon></button></td>}
          {columns.map(c=><td key={c.key} className="datagrid-cell" style={{height:rh}}>{c.render?c.render(r):r[c.key]}</td>)}
        </tr>
        {expandable&&isOpen&&<tr className="datagrid-row datagrid-detail-row"><td className="datagrid-cell" colSpan={nCols}>{renderDetail?renderDetail(r):null}</td></tr>}
      </React.Fragment>;})}
      </tbody>
    </table>
    {view.length===0&&<div className="datagrid-placeholder"><clr-icon shape="search" size="32"></clr-icon>{placeholder}</div>}
    <div className="datagrid-footer">
      <span className="datagrid-footer-description">{footerText||(sel.size?sel.size+' selected · ':'')+data.length+' items'}</span>
      {pageSize>0&&<div className="datagrid-pagination">
        <button onClick={()=>setPage(0)} disabled={page===0} aria-label="First">«</button>
        <button onClick={()=>setPage(p=>Math.max(0,p-1))} disabled={page===0} aria-label="Previous">‹</button>
        <span className="pagination-current">{page+1} / {pages}</span>
        <button onClick={()=>setPage(p=>Math.min(pages-1,p+1))} disabled={page===pages-1} aria-label="Next">›</button>
        <button onClick={()=>setPage(pages-1)} disabled={page===pages-1} aria-label="Last">»</button>
      </div>}
    </div>
  </div>;
}
