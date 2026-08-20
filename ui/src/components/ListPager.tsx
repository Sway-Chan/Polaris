import { useTranslation } from 'react-i18next';

export interface PageWindow {
  page: number;
  pageCount: number;
  start: number;
  end: number;
}

/**
 * 把完整结果集投影成一个有界页面。数据仍由调用方完整持有；这里只限制实际挂载的 DOM。
 */
export function pageWindow(total: number, requestedPage: number, pageSize: number): PageWindow {
  if (!Number.isInteger(pageSize) || pageSize <= 0) {
    throw new RangeError();
  }
  const safeTotal = Number.isFinite(total) ? Math.max(0, Math.trunc(total)) : 0;
  const pageCount = Math.max(1, Math.ceil(safeTotal / pageSize));
  const candidate = Number.isFinite(requestedPage) ? Math.trunc(requestedPage) : 0;
  const page = Math.min(pageCount - 1, Math.max(0, candidate));
  const start = page * pageSize;
  return {
    page,
    pageCount,
    start,
    end: Math.min(safeTotal, start + pageSize),
  };
}

interface ListPagerProps extends PageWindow {
  total: number;
  onPageChange: (page: number) => void;
}

/** 连接与日志共用的有界列表导航；文案只从 common i18n 读取。 */
export function ListPager({
  page,
  pageCount,
  start,
  end,
  total,
  onPageChange,
}: ListPagerProps) {
  const { t } = useTranslation();
  if (pageCount <= 1) return null;

  return (
    <div className="list-pager" aria-live="polite">
      <span>{t('common.pageStatus', { start: start + 1, end, total })}</span>
      <div className="list-pager-actions">
        <button
          type="button"
          className="btn ghost list-page-btn"
          disabled={page === 0}
          onClick={() => onPageChange(page - 1)}
        >
          {t('common.previousPage')}
        </button>
        <button
          type="button"
          className="btn ghost list-page-btn"
          disabled={page >= pageCount - 1}
          onClick={() => onPageChange(page + 1)}
        >
          {t('common.nextPage')}
        </button>
      </div>
    </div>
  );
}
