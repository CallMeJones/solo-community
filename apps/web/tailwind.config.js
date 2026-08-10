/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        // Okabe-Ito-inspired colorblind-safe palette for node kinds.
        kind: {
          episode: '#f2b35d', // dune gold
          document: '#d56f3e', // spice copper
          chunk: '#b487ff', // arcane violet
          cluster: '#65d6a3', // oasis green
          entity: '#f7df8a', // parchment yellow
        },
      },
      fontFamily: {
        sans: [
          'ui-sans-serif',
          'system-ui',
          '-apple-system',
          'BlinkMacSystemFont',
          'Segoe UI',
          'Roboto',
          'sans-serif',
        ],
        mono: ['ui-monospace', 'Menlo', 'Consolas', 'Liberation Mono', 'monospace'],
      },
    },
  },
  plugins: [],
};
