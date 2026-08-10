import type { ButtonHTMLAttributes } from 'react';

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: 'default' | 'primary' | 'ghost';
}

export function Button({ className = '', variant = 'default', ...rest }: ButtonProps) {
  const base =
    'inline-flex items-center justify-center rounded-md px-3 py-1.5 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed';
  const variants: Record<NonNullable<ButtonProps['variant']>, string> = {
    default: 'bg-slate-800 text-slate-100 hover:bg-slate-700',
    primary: 'bg-sky-700 text-white hover:bg-sky-600',
    ghost: 'bg-transparent text-slate-300 hover:bg-slate-800 hover:text-slate-100',
  };
  return <button className={`${base} ${variants[variant]} ${className}`} {...rest} />;
}
