import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

export function ResultPagination({ page, total, pageSize, root, disabled, onPage }: {
  page: number; total: number; pageSize: number; root: string | null;
  disabled: boolean; onPage: (page: number) => void;
}) {
  const { t } = useTranslation();
  const pages = Math.max(1, Math.ceil(total / pageSize));
  const [input, setInput] = useState(String(page + 1));
  useEffect(() => setInput(String(page + 1)), [page]);
  return <div className="result-pagination">
    {root !== null && <span className="result-root" title={root}>{t('pagination.root', { root })}</span>}
    <button disabled={disabled || page === 0} onClick={() => onPage(0)}>{t('pagination.first')}</button>
    <button disabled={disabled || page === 0} onClick={() => onPage(page - 1)}>{t('pagination.previous')}</button>
    <span>{t('pagination.summary', { page: page + 1, pages, total, pageSize })}</span>
    <button disabled={disabled || page + 1 >= pages} onClick={() => onPage(page + 1)}>{t('pagination.next')}</button>
    <button disabled={disabled || page + 1 >= pages} onClick={() => onPage(pages - 1)}>{t('pagination.last')}</button>
    <form onSubmit={(event) => {
      event.preventDefault();
      const target = Number(input);
      if (Number.isSafeInteger(target) && target >= 1 && target <= pages) onPage(target - 1);
    }}>
      <input type="number" min={1} max={pages} value={input} disabled={disabled}
        aria-label={t('pagination.jump')} onChange={(event) => setInput(event.target.value)} />
      <button disabled={disabled}>{t('pagination.jump')}</button>
    </form>
  </div>;
}
